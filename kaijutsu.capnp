@0xb8e3f4a9c2d1e0f7;

# Kaijutsu RPC Schema
# Cap'n Proto interface for kernel-based collaborative spaces

# ============================================================================
# Core identity and primitives
# ============================================================================
# Distributed tracing, who is talking, and the block address every other type is keyed by.

# W3C Trace Context for distributed tracing across SSH boundary.
# See: https://www.w3.org/TR/trace-context/
struct TraceContext {
  traceparent @0 :Text;   # e.g. "00-4bf92f3577b6a8141fea3e3b0aa7d3f5-00f067aa0ba902b7-01"
  tracestate @1 :Text;    # vendor-specific key=value pairs (optional, may be empty)
}

struct Identity {
  username @0 :Text;
  displayName @1 :Text;
  principalId @2 :Data;   # 16-byte PrincipalId (UUIDv7)
}

# Block identifier — globally unique across all contexts and agents.
# Uses typed UUIDs (binary, 16 bytes each) instead of strings.
struct BlockId {
  contextId @0 :Data;   # 16-byte ContextId (UUIDv7)
  principalId @1 :Data;     # 16-byte PrincipalId (UUIDv7)
  seq @2 :UInt64;
}

# ============================================================================
# Block vocabulary
# ============================================================================
# What a block is, how it is queried, and how it is filtered — the shared nouns everything below spells with.

# Role in conversation (User/Model terminology for collaborative peer model)
enum Role {
  user @0;
  model @1;
  system @2;
  tool @3;
  asset @4;
}

# Execution/processing status
enum Status {
  pending @0;
  running @1;
  done @2;
  error @3;
  # An unsubmitted compose draft. A draft is a real block owned by one
  # principal; submitting it flips this status rather than copying the text
  # into a new block, which is what makes submit incapable of losing what you
  # typed. Never hydrated, never counted as live work.
  draft @4;
}

# Block content type — 10 variants covering what a block *is*.
# Mechanism metadata lives in companion enums:
# - ToolKind on ToolCall/ToolResult: which execution engine (Shell, Mcp, Builtin)
# - DriftKind on Drift: how content transferred (Push, Pull, Merge, Distill)
# - ErrorPayload on Error: structured diagnostics
# - NotificationPayload on Notification: broker-emitted tool/log events
# - ResourcePayload on Resource: MCP resource contents
enum BlockKind {
  text @0;
  thinking @1;
  toolCall @2;
  toolResult @3;
  drift @4;
  file @5;
  error @6;
  notification @7;
  resource @8;
  # Operator/UI telemetry — rc stdout, hook output, kernel diagnostics.
  # Hydrator skips unconditionally so the LLM never sees them.
  trace @9;
  # Groomable task/plan item (household-agent grooming). Lifecycle rides
  # `taskStatus` (a Text field below, same convention as `contentType` —
  # no dedicated capnp enum). Subtasks reuse `parentId`.
  task @10;
}

# Which execution engine handled a tool call/result.
enum ToolKind {
  shell @0;     # kaish shell execution (shell_execute RPC)
  mcp @1;       # MCP tool invocation (via registered MCP server)
  builtin @2;   # Kernel builtin tool (no external process)
}

# How a drift block arrived from another context.
enum DriftKind {
  push @0;          # User manually pushed content
  pull @1;          # User pulled/requested content
  merge @2;         # Context merge (fork coming home)
  distill @3;       # LLM-summarized before transfer
  notification @4;  # External notification (MCP resource updates, system events)
  fork @5;          # Fork marker (ephemeral, summarizes the fork operation)
}

# Submit routing: explicit mode instead of prefix-sniffing.
enum InputMode {
  chat @0;
  shell @1;
}

# Flat block snapshot — all fields present, some unused depending on kind.
struct BlockSnapshot {
  # Identity. Author (who PLAYED) is derived from id.principalId — there is no
  # separate author field. The hyoushigi `track` below is the LANE (where the
  # clip lives), never the author; one track's blocks span multiple principals
  # (player + beat()).
  id @0 :BlockId;

  # DAG structure
  parentId @1 :BlockId;
  hasParentId @2 :Bool;          # True if parentId is set

  # Classification
  role @3 :Role;
  status @4 :Status;
  kind @5 :BlockKind;
  createdAt @6 :UInt64;          # Unix timestamp in milliseconds

  # Content (all blocks)
  content @7 :Text;              # Main text content
  contentType @8 :Text;         # MIME hint (e.g. "text/markdown"); empty = heuristic detection
  collapsed @9 :Bool;           # For thinking blocks

  # Hydration control
  # ephemeral: displayed but excluded from LLM hydration (system-managed —
  #   human-only output like help text/status that would waste model context).
  # excluded: user-curated exclusion, toggled during staging.
  ephemeral @10 :Bool;
  excluded @11 :Bool;

  # Tool call (toolCall)
  toolName @12 :Text;            # Tool name for toolCall
  toolInput @13 :Text;           # JSON-encoded input for toolCall
  toolUseId @14 :Text;           # LLM-assigned invocation ID ("toolu_01ABC…") — on ToolCall + ToolResult
  toolKind @15 :ToolKind;        # Which execution engine (shell/mcp/builtin)
  hasToolKind @16 :Bool;         # True if toolKind is set (enum default 0=shell is valid)

  # Tool result (toolResult)
  toolCallId @17 :BlockId;       # Reference to parent toolCall block
  exitCode @18 :Int32;           # Exit code for toolResult
  hasExitCode @19 :Bool;         # True if exitCode is set (Int32 value type)
  isError @20 :Bool;             # True if toolResult is an error
  stderr @21 :Text;              # Standard error stream — persisted separately from content (stdout)
  hasStderr @22 :Bool;           # True if stderr is set (distinguishes "" from unset)

  # Structured output data for richer formatting (typed Cap'n Proto struct)
  outputData @23 :OutputData;

  # File metadata (file blocks)
  filePath @24 :Text;            # Path for file-kind blocks (e.g. "/src/main.rs")

  # Drift (cross-context transfer)
  sourceContext @25 :Data;       # 16-byte ContextId of originating context
  sourceModel @26 :Text;         # Model that produced this content
  driftKind @27 :DriftKind;      # How this block arrived (push/pull/merge/distill)
  hasDriftKind @28 :Bool;        # True if driftKind is set (enum default 0=push is valid)

  # Error payload (error blocks)
  errorPayload @29 :ErrorPayload;
  hasErrorPayload @30 :Bool;

  # Notification payload (notification blocks)
  notificationPayload @31 :NotificationPayload;
  hasNotificationPayload @32 :Bool;

  # Resource payload (resource blocks)
  resourcePayload @33 :ResourcePayload;
  hasResourcePayload @34 :Bool;

  # Hyoushigi timeline — set only on blocks materialized from a committed
  # timeline cell; absent on every ordinary block. `tick` is the timeline
  # coordinate; `track` is the stable lane identity (DAW sense), never the
  # author (author stays id.principalId).
  tick @35 :Int64;
  hasTick @36 :Bool;             # True if tick is set (Int64 value type)
  track @37 :Text;
  hasTrack @38 :Bool;            # True if track is set (None ⇔ hasTrack=false)

  # Reasoning-continuity token for Thinking blocks — opaque, provider-agnostic
  # "rehydratable" marker: verbatim Anthropic/Gemini signature, or a provider
  # nonce. Absent on non-reasoning and legacy/older-wire blocks.
  signature @39 :Text;
  hasSignature @40 :Bool;        # True if signature is set (distinguishes "" from unset)

  # Task lifecycle status (task blocks). "" falls back to "open" — same
  # "empty = default" convention as contentType (no dedicated capnp enum).
  taskStatus @41 :Text;
}

# Scalar block metadata carried by onBlockMetadataChanged.
#
# These fields are set (typically once) after a block is inserted — e.g. the
# real exit code and stderr written when a shell command finishes. They are
# NOT DTE-tracked (unlike content), so they ride this event and are applied
# directly to the client's store replica, independent of the text frontier.
struct BlockMetadata {
  exitCode @0 :Int32;
  hasExitCode @1 :Bool;       # True if exitCode is set
  isError @2 :Bool;
  contentType @3 :Text;       # MIME type; empty falls back to heuristic
  ephemeral @4 :Bool;
  toolUseId @5 :Text;         # LLM-assigned tool invocation id ("" if unset)
  stderr @6 :Text;            # Standard error stream
  hasStderr @7 :Bool;         # True if stderr is set (distinguishes "" from unset)
  # Task lifecycle status (BlockKind::Task only); "" falls back to "open",
  # same "empty = default" convention as contentType.
  taskStatus @8 :Text;
}

# Query for fetching blocks — union of all/byIds/byFilter
struct BlockQuery {
  union {
    all @0 :Void;
    byIds @1 :List(BlockId);
    byFilter @2 :BlockFilter;
  }
}

# Filter criteria for block queries
struct BlockFilter {
  kinds @0 :List(BlockKind);
  hasKinds @1 :Bool;
  roles @2 :List(Role);
  hasRoles @3 :Bool;
  statuses @4 :List(Status);
  hasStatuses @5 :Bool;
  # Retired 2026-08-02 with the dead `compacted` block flag it filtered on.
  # Cap'n Proto ordinals must stay sequential, so the slot is kept — unread,
  # unwritten — rather than reused.
  unusedExcludeCompacted @6 :Bool;
  limit @7 :UInt32;
  maxDepth @8 :UInt32;
  parentId @9 :BlockId;
  hasParentId @10 :Bool;
}

# Server-side filter for block event subscriptions.
# Empty lists = unconstrained. All constraints are AND'd.
struct BlockEventFilter {
  contextIds @0 :List(Data);         # 16-byte ContextId UUIDs
  hasContextIds @1 :Bool;            # Needed because empty list ≠ unconstrained
  eventTypes @2 :List(BlockFlowKind);
  hasEventTypes @3 :Bool;
  blockKinds @4 :List(BlockKind);
  hasBlockKinds @5 :Bool;
}

# Discriminant for BlockFlow event variants (for subscription filtering).
enum BlockFlowKind {
  inserted @0;
  deleted @1;
  statusChanged @2;
  collapsedChanged @3;
  moved @4;
  outputChanged @5;
  metadataChanged @6;
  contextSwitched @7;
  excludedChanged @8;
  # A render directive, not a block-level event — never meaningfully
  # filterable by context/kind (see `BlockFlow::matches_filter`'s unconditional
  # bypass for `RenderCue`). Kept 1:1 with the Rust `BlockFlowKind` enum anyway
  # so the two never drift silently out of sync.
  renderCue @9;
  # A low-rate beat reference for the sink's continuous timebase (the metronome
  # phasor, docs/midi.md "The relative-lead timebase, analyzed"). Also a
  # directive — bypasses `matches_filter` like `renderCue`.
  beatSync @10;
  # Classified text changes (docs/change-feed.md). The kernel decides append
  # vs replace inside its mutation lock and publishes one of these instead of
  # opaque operation bytes (the raw-ops wire event these replaced was deleted
  # in the 2026-08-15 flag day). Filterable like any other block-level event.
  textAppended @11;
  textReplaced @12;
}

# ============================================================================
# Errors and notifications
# ============================================================================
# Structured payloads for Error, Notification, and Resource blocks (BlockSnapshot's kind-specific fields).

# What system produced the error.
enum ErrorCategory {
  tool @0;
  stream @1;
  rpc @2;
  render @3;
  parse @4;
  validation @5;
  kernel @6;
}

# How severe the error is.
enum ErrorSeverity {
  warning @0;
  error @1;
  fatal @2;
}

# Structured error payload for Error blocks.
struct ErrorPayload {
  category @0 :ErrorCategory;
  severity @1 :ErrorSeverity;
  code @2 :Text;              # Stable machine-readable ID (empty = None)
  detail @3 :Text;            # Full diagnostic text (empty = None)
  hasSpan @4 :Bool;
  spanLine @5 :UInt32;
  spanColumn @6 :UInt32;
  spanLength @7 :UInt32;
  hasSourceKind @8 :Bool;
  sourceKind @9 :BlockKind;
}

# What a notification is about.
enum NotificationKind {
  toolAdded @0;
  toolRemoved @1;
  log @2;
  promptsChanged @3;
  coalesced @4;
}

# Severity for Log notifications. Kept in sync with kaijutsu-types::LogLevel.
enum LogLevel {
  trace @0;
  debug @1;
  info @2;
  warn @3;
  error @4;
}

# Structured notification payload for Notification blocks.
struct NotificationPayload {
  instance @0 :Text;            # MCP instance id (never empty)
  kind @1 :NotificationKind;
  hasLevel @2 :Bool;            # True iff `level` applies (Log events)
  level @3 :LogLevel;
  tools @4 :List(Text);         # ToolAdded/Removed — names in the batch (1+); empty otherwise
  hasCount @5 :Bool;            # True iff `count` applies (Coalesced)
  count @6 :UInt32;
  detail @7 :Text;              # Expandable body — empty = None
}

# Structured resource payload for Resource blocks.
# Exactly one of `text`/`blobBase64` is populated per read, signaled by the
# corresponding `has*` flag. Binary bodies stay base64-encoded end-to-end.
struct ResourcePayload {
  instance @0 :Text;                  # MCP instance id (never empty)
  uri @1 :Text;
  mimeType @2 :Text;                  # empty unless hasMimeType
  hasMimeType @3 :Bool;
  size @4 :UInt64;                    # 0 unless hasSize
  hasSize @5 :Bool;
  text @6 :Text;                      # empty unless hasText
  hasText @7 :Bool;
  blobBase64 @8 :Text;                # empty unless hasBlob
  hasBlob @9 :Bool;
  hasParentResourceBlockId @10 :Bool;
  parentResourceBlockId @11 :BlockId;
}

# ============================================================================
# The context change feed (docs/change-feed.md)
# ============================================================================
#
# One ordered feed per context, replacing the thirteen separate `BlockEvents`
# methods and — the point of the exercise — the serialized diamond-types
# operations they carried. A client applies domain facts here; it never decodes
# a storage engine's encoding of them, and therefore never links a text engine.
#
# `BlockEvents` still exists while the clients migrate. It goes away with them,
# in a flag day; see the "Deleted by this change" table in the design.

# Text added at the END of a block's text. The kernel decided this was an
# append while it held the mutation lock — coordinates only, never the name of
# the tool or function that made the edit — so the client's whole job is to
# append `suffix` to what it already holds.
struct TextAppend {
  blockId @0 :BlockId;
  suffix @1 :Text;
}

# A block's text changed in a way that was not an append. Carries the whole
# after-text: an insert in the middle, a delete, and a splice are all this.
# NEVER compare character counts to decide whether anything changed — a replace
# can keep the count, and ACP rendered corrupted text by making exactly that
# assumption.
struct TextReplace {
  blockId @0 :BlockId;
  content @1 :Text;
}

# A block was authored, with the position it landed at.
#
# The position is not redundant with the snapshot: the wire `BlockSnapshot`
# carries no ordering key, so a client that received only the snapshot would
# know the block exists and not where it goes. `hasAfterId` false means "at
# the beginning".
struct BlockInsert {
  block @0 :BlockSnapshot;
  afterId @1 :BlockId;
  hasAfterId @2 :Bool;
}

# A block moved. `hasAfterId` false means "to the beginning".
struct BlockMove {
  blockId @0 :BlockId;
  afterId @1 :BlockId;
  hasAfterId @2 :Bool;
}

struct BlockStatusChange {
  blockId @0 :BlockId;
  status @1 :Status;
}

# One boolean flag flipped. Which flag is carried by the `ContextEvent` union
# arm (`collapsedChanged` / `excludedChanged`), not by a field here, so an
# unhandled arm is a compile error rather than a silently ignored flag name.
struct BlockFlagChange {
  blockId @0 :BlockId;
  value @1 :Bool;
}

struct BlockMetadataChange {
  blockId @0 :BlockId;
  metadata @1 :BlockMetadata;
}

struct BlockOutputChange {
  blockId @0 :BlockId;
  output @1 :OutputData;
}

# One accepted change to a context. Deliberately a union of domain facts:
# blocks authored, edited, completed, excluded — Kaijutsu's vocabulary, not a
# text engine's.
struct ContextEvent {
  # The context version this ONE change brought the context to.
  #
  # Not redundant with the delivery's version, which is only the last one in
  # the batch. A client filters per event, because a batch can straddle
  # anything: a snapshot served in the middle of a burst means some events in
  # a delivery are already in the snapshot and some are not, and a per-delivery
  # comparison has to choose all-or-nothing. Choosing "all" duplicates text
  # (a batch of v5,v6,v7 against a snapshot at v6 replays v5 and v6); choosing
  # "none" loses v7. Per-event versions make the question answerable.
  version @0 :UInt64;

  union {
    blockInserted @1 :BlockInsert;
    blockDeleted @2 :BlockId;
    blockMoved @3 :BlockMove;
    textAppended @4 :TextAppend;
    textReplaced @5 :TextReplace;
    statusChanged @6 :BlockStatusChange;
    collapsedChanged @7 :BlockFlagChange;
    excludedChanged @8 :BlockFlagChange;
    metadataChanged @9 :BlockMetadataChange;
    outputChanged @10 :BlockOutputChange;
  }
}

# The client's end of the feed.
interface ContextObserver {
  # Next free ordinal: 2. Ordinals are dense and permanent — never
  # reuse one, and never renumber outside a flag day; retiring a method
  # leaves a `retiredNN @NN ();` stub instead.

  # One delivery carries an ordered batch of changes and the version they
  # bring the client to: applying `events` in order makes the client's state
  # exactly the context at `version`.
  #
  # Batching is native here — a burst of streamed tokens is one call, and a
  # tool's final output text and its `Done` status arrive together instead of
  # racing.
  #
  # Order is the contract. Events are in kernel-accepted order across ALL
  # blocks in the delivery; an event for one block is never moved ahead of an
  # earlier event for another. A delivery is a window, never a reordering.
  #
  # Timing artifacts (render cues, beat sync) do NOT ride this feed. Batching
  # trades latency for fewer messages and docs/midi.md forbids that trade for
  # anything on the musical timebase.
  onContextChanged @0 (contextId :Data, events :List(ContextEvent),
                       version :UInt64);

  # This feed is over and was not resumable — the subscriber could not keep up,
  # or the server is going away. `deliveredVersion` is the last version this
  # observer was actually brought to. Do not patch forward from it: re-subscribe
  # and fetch a fresh snapshot with `getBlocks` (which returns its own version).
  onTerminated @1 (reason :SubscriptionEndReason, deliveredVersion :UInt64);
}

# Snapshot of a context version for timeline navigation
struct VersionSnapshot {
  version @0 :UInt64;         # Context version number
  timestamp @1 :UInt64;       # Unix millis when this version was created
  blockCount @2 :UInt32;      # Number of blocks at this version
  changeKind @3 :ChangeKind;
  changedBlockId @4 :BlockId; # The block that changed (if applicable)
}

# What changed at a given version
enum ChangeKind {
  blockAdded @0;
  blockDeleted @1;
  edit @2;
  statusChange @3;
}

# Why a push subscription ended. Additive: an unknown enumerant decodes as the
# default, which is the honest "we don't know" answer.
enum SubscriptionEndReason {
  # The subscriber's bounded queue filled — it could not keep up.
  slowSubscriber @0;
  # The server is going away (shutdown, kernel restart).
  serverShutdown @1;
  # Replaced by a newer subscription from the same client instance.
  superseded @2;
  # The server cannot produce a correct stream and is ending it rather than
  # sending one it knows is wrong (docs/change-feed.md). Nothing is wrong with
  # the client; recovery is the same refetch as any other ending.
  internalFault @3;
}

# ============================================================================
# Block events
# ============================================================================
# The BlockEvents push channel — superseded by the change feed above, still live until every client migrates.

# Callback for receiving block updates from server
#
# # Delivery contract (2026-08-05 backpressure rework)
#
# Every method carries `subSeq`: a per-SUBSCRIPTION monotonic counter starting
# at 1, incremented once per callback the server issues on this subscription.
# The kernel's FlowBus is lossless per subscriber — a subscriber that cannot
# keep up is terminated, not silently truncated — so a gap in `subSeq` can
# only mean one of:
#
#   * you were terminated for falling behind (`onSubscriptionTerminated`
#     arrives, and the connection is dropped right after), or
#   * you resubscribed / reconnected and the counter restarted at 1.
#
# It can NEVER mean "the server quietly lost an event". A client that sees a
# gap without a termination has found a kernel bug and should say so loudly.
#
# `subSeq` is 0 on a peer that predates the rework; treat 0 as "not reporting".
# Methods on the musical-time lane (`onRenderCue`, `onBeatSync`) count on their
# OWN sequence, because a missed beat is missed on purpose (docs/midi.md) and
# must not look like a hole in the block stream.
interface BlockEvents {
  # Next free ordinal: 13. Ordinals are dense and permanent — never
  # reuse one, and never renumber outside a flag day; retiring a method
  # leaves a `retiredNN @NN ();` stub instead.

  # `ops` is retired: the server always sends an empty list and no client
  # reads it. Removing the field renumbers `subSeq`, so it waits for a flag
  # day.
  onBlockInserted @0 (contextId :Data, block :BlockSnapshot, afterId :BlockId, hasAfterId :Bool, ops :Data, subSeq :UInt64);
  onBlockDeleted @1 (contextId :Data, blockId :BlockId, subSeq :UInt64);
  onBlockCollapsed @2 (contextId :Data, blockId :BlockId, collapsed :Bool, subSeq :UInt64);
  onBlockMoved @3 (contextId :Data, blockId :BlockId, afterId :BlockId, hasAfterId :Bool, subSeq :UInt64);
  onBlockStatusChanged @4 (contextId :Data, blockId :BlockId, status :Status, subSeq :UInt64);

  # Session control events (server → client)
  onContextSwitched @5 (contextId :Data, subSeq :UInt64);

  # Block excluded flag changed (staging curation)
  onBlockExcludedChanged @6 (contextId :Data, blockId :BlockId, excluded :Bool, subSeq :UInt64);

  # Scalar metadata changed (exit code, stderr, content type, …). Applied
  # directly to the client store — frontier-independent, so it survives a
  # reconnect even when text ops are gated behind a full resync.
  onBlockMetadataChanged @7 (contextId :Data, blockId :BlockId, metadata :BlockMetadata, subSeq :UInt64);

  # Structured output data changed (output is not DTE-tracked, so it rides
  # its own event rather than the block text op stream).
  onBlockOutputChanged @8 (contextId :Data, blockId :BlockId, output :OutputData, subSeq :UInt64);

  # Render a cue (docs/pcm.md, docs/midi.md "Render is a wire cue"). A kernel
  # directive (from `kj play`, later the track render seam), not a block change
  # — carried on this channel because it already fans out to every attached
  # client. `contextId` is the originating context (reserved for future
  # per-listener routing); the standalone slice forwards unconditionally.
  #
  # Musical-time lane: `subSeq` counts separately from the block stream and a
  # gap is legitimate — a stalled sink misses cues rather than replaying stale
  # ones.
  onRenderCue @9 (contextId :Data, cue :RenderCue, subSeq :UInt64);

  # A beat reference for the sink's continuous timebase (docs/midi.md "The
  # relative-lead timebase, analyzed"). Emitted at a low rate while a track's
  # clock rolls; the sink's phasor extrapolates the beats between and slews
  # toward each reference. Like onRenderCue, a directive that fans out to every
  # attached client, on the musical-time lane. `contextId` is the track's score
  # context (the same key onRenderCue uses), so a sink can associate a beat
  # with its track.
  onBeatSync @10 (contextId :Data, beatRef :BeatRef, subSeq :UInt64);

  # A bounded MIDI request/reply at one sink (docs/midi-next.md "SysEx: the
  # exchange pattern"). The FIRST round-trip member of this otherwise
  # fire-and-forget set: onRenderCue/onBeatSync push, this one *calls*. Cap'n
  # Proto is bidirectional and promise-returning, so the kernel asks a
  # specific sink a question and awaits its answer over the connection that
  # sink already holds.
  #
  # Unlike every method above, this is NOT a fan-out: the kernel picks the ONE
  # connection whose sink reported the device present (the presence store's
  # SinkAttribution) and calls it there. Sending a device dialogue to every
  # attached client would mean several ears answering for gear only one of
  # them has.
  #
  # - `portOrDevice` — a device profile name (`keystep-pro`, resolved through
  #   the sink's own routing table, exactly as a device-addressed control cue
  #   is) or a backend port address (`"24:0"`) when the caller already knows
  #   the port. The kernel sends the device name; it never learns ALSA
  #   numbers.
  # - `payload` — complete MIDI bytes to send (a whole F0…F7 SysEx today).
  # - `replyMatch` — leading bytes the reply must start with (`F0 7E` for a
  #   universal Identity Reply); traffic that doesn't match is ignored, so a
  #   chatty device can't answer the wrong question.
  # - `timeoutMs` — the sink's own bound on the wait. The kernel enforces a
  #   slightly larger overall deadline so a wedged sink can't hang the caller.
  #
  # Errors are LOUD: unplugged, unroutable, no reply before the timeout, or a
  # sink with no exchange client all come back as a failed call, never as an
  # empty reply and never as a hang.
  exchange @11 (portOrDevice :Text, payload :Data, replyMatch :Data, timeoutMs :UInt32)
      -> (reply :Data);

  # THE LAG KICK. This subscription is over: the client could not drain its
  # per-subscriber queue fast enough, so the kernel terminated it rather than
  # let it keep a hole-punched view of the log (Amy, 2026-08-05: "I'd rather be
  # disconnected"). Sent best-effort with a short deadline, immediately before
  # the server tears the RPC connection down — so a client that never
  # implements this method still recovers, via the ordinary reconnect path,
  # just without knowing why.
  #
  # The contract for the client: this is NOT a resumable gap. Reconnect,
  # re-subscribe, and re-fetch a snapshot (`Kernel.getBlocks` for the block
  # feed); do not try to patch forward from the last `subSeq`, because the
  # events between are gone by construction.
  #
  # - `reason` — why the subscription ended.
  # - `topic` — the FlowBus topic whose event could not be enqueued.
  # - `delivered` — how many events this subscription successfully received;
  #   equals the last `subSeq` the client should have seen.
  # - `capacity` — the queue depth that was exceeded, for the operator.
  onSubscriptionTerminated @12 (reason :SubscriptionEndReason, topic :Text,
                                delivered :UInt64, capacity :UInt64);
}

# ============================================================================
# Editor
# ============================================================================
# Renderer-facing state for the in-app vi editor (docs/vi.md).

# Renderer-facing snapshot of an in-app editor session (the vi/edit builtin).
# Carries everything a renderer draws; see docs/vi.md.
struct EditorState {
  session @0 :UInt64;
  text @1 :Text;
  cursor @2 :UInt64;     # char offset of the cursor
  mode @3 :Text;         # vim mode label; "" = none/normal
  dirty @4 :Bool;        # buffer differs from the last open/save checkpoint
  commandLine @5 :Text;  # the ":"-line while command mode is active (":wq"); "" = bar unfocused
  message @6 :Text;      # transient status/error line (vim E492); "" = none
}

# Callback for receiving editor-session state pushes (the in-app vi editor).
# The push channel exists so a peer's write to an open block reaches every
# renderer the instant it lands — collaborative editing, not poll lag.
interface EditorEvents {
  # Next free ordinal: 2. Ordinals are dense and permanent — never
  # reuse one, and never renumber outside a flag day; retiring a method
  # leaves a `retiredNN @NN ();` stub instead.

  onEditorState @0 (state :EditorState);
  onEditorClosed @1 (sessionId :UInt64);
}

# ============================================================================
# Turn lifecycle push channel
# ============================================================================
# Turn completion is the one fact about a turn that every client needs and no
# client can compute. Before this channel, clients inferred it by polling block
# status — a heuristic that cannot tell "finished" from "cancelled" from "still
# thinking", and that gets slower and less certain as a turn gets longer.
#
# Mirrors kernel `TurnFlow::Completed`/`Failed` (kaijutsu-kernel/src/flows.rs);
# `TurnFlow::Requested` deliberately stays in-process (it is a request TO the
# server's turn driver, which nothing outside the process can serve).

# Why a turn stopped. 1:1 with kernel `TurnStopReason` and with the ACP v1
# `stopReason` vocabulary — end_turn / cancelled / max_tokens /
# max_turn_requests. The soft/hard cancel split matches `interruptContext`'s
# `immediate` flag: a soft cancel lets the in-flight model call finish (whole
# phrase), a hard one aborts mid-token (fragment).
enum TurnStopReason {
  endTurn @0;             # model finished; ACP end_turn
  cancelledSoft @1;       # interruptContext(immediate = false); ACP cancelled
  cancelledImmediate @2;  # interruptContext(immediate = true);  ACP cancelled
  maxTokens @3;           # provider hit the output ceiling; ACP max_tokens
  maxIterations @4;       # agentic tool-iteration cap; ACP max_turn_requests
}

# Who asked for the turn. Interactive turns announce like every other turn;
# consumers that must ignore them (the kernel's own OODA Act) filter here.
enum TurnOrigin {
  interactive @0;   # a player submitted a prompt (prompt / submitInput)
  autonomous @1;    # the kernel drove itself (kj fork --prompt, drift, cruise)
}

# Callback for receiving turn-outcome pushes. Kernel-wide, exactly like
# `subscribeEditor` — the event names its context rather than the subscription
# filtering to one, so a client watching several contexts needs one channel.
interface TurnEvents {
  # Next free ordinal: 2. Ordinals are dense and permanent — never
  # reuse one, and never renumber outside a flag day; retiring a method
  # leaves a `retiredNN @NN ();` stub instead.

  # A turn reached an ending the driver arrived at on purpose. `outputBlockId`
  # is the model's last text block; `hasOutputBlock` is false (and the id
  # meaningless) when the turn produced no text at all.
  onTurnCompleted @0 (contextId :Data, principalId :Data,
                      outputBlockId :BlockId, hasOutputBlock :Bool,
                      stopReason :TurnStopReason, origin :TurnOrigin);
  # A turn BROKE — hydration failure, provider/stream error. Not a cancel:
  # a cancelled turn arrives as onTurnCompleted with a cancelled stopReason.
  onTurnFailed @1 (contextId :Data, principalId :Data, error :Text,
                   origin :TurnOrigin);
}

# ============================================================================
# Contexts
# ============================================================================
# A context's identity and metadata, plus staged and dead-lettered cross-context drift.

# A context within a kernel
struct Context {
  id @0 :Data;                    # 16-byte ContextId (UUIDv7)
  label @1 :Text;                 # Optional human-friendly name
}

# Information about a registered context (used by listContexts, KernelInfo)
struct ContextHandleInfo {
  id @0 :Data;                    # 16-byte ContextId (UUIDv7) — the real identity
  label @1 :Text;                 # Optional human-friendly name (mutable)
  parentId @2 :Data;              # 16-byte ContextId of parent (empty = no parent)
  provider @3 :Text;
  model @4 :Text;
  createdAt @5 :UInt64;
  traceId @6 :Data;               # 16-byte OTel trace ID for context-scoped tracing
  forkKind @7 :Text;              # "full"/"shallow"/"compact"/"subtree" or empty
  archivedAt @8 :UInt64;          # 0 = active, else Unix millis when archived
  keywords @9 :List(Text);        # Synthesis keywords (empty if not yet synthesized)
  topBlockPreview @10 :Text;      # Preview of most representative block (empty if none)
  contextState @11 :Text;         # "live"/"staging"/"concluded"/"archived" (default "live")
  contextType @12 :Text;          # rc bucket / mode bundle (default "default")
  concludedAt @13 :UInt64;        # 0 = not concluded, else Unix millis of the explicit `conclude` act
  liveStatus @14 :Status;         # live activity: running (working) / error (last turn failed) / else idle
  lastActivityAt @15 :UInt64;     # Unix millis of the most recent block append/mutation; 0 = never/unknown
  trackId @16 :Text;              # Track this context is attached to (empty = unattached; TrackIds are never empty)
  promotedAt @17 :UInt64;         # 0 = not promoted, else Unix millis of the explicit ring-0 promote (append-ordered, never restamped)
  demotedAt @18 :UInt64;          # 0 = not demoted, else Unix millis of the explicit push to the demoted ring
  pausedAt @19 :UInt64;           # 0 = not paused, else Unix millis of the explicit pause; gating deferred (see setContextPaused)

  # Context-window usage — the bottom-dock gauge's wire spine. Resolved
  # kernel-side from the SAME KernelDb::get_context_usage +
  # LlmRegistry::context_window_for + shared percentage helper that
  # `kj context info --json` uses, so the two surfaces can never disagree.
  # `contextUsedTokens` is the LAST completed call's fill (input+output),
  # NOT a running total across turns — see `ContextUsageRow`.
  contextWindow @20 :UInt64;         # 0 = unknown (no configured window for this model, or no usage yet)
  contextUsedTokens @21 :UInt64;         # 0 = no usage yet (this context has never completed an LLM call)
  contextUsedPct @22 :Float32 = -1.0; # -1.0 = unknown (window unconfigured or no usage yet). This struct's
                                         # usual "0 = unknown" convention doesn't work here because 0.0 is a
                                         # legitimate value (a fresh context is genuinely 0% used), so -1.0 is
                                         # the dedicated sentinel — never clamped, never fabricated. The schema
                                         # default is -1.0 (not capnp's usual 0), so a builder that never calls
                                         # setContextUsedPct decodes as "unknown", never a false Some(0.0).

  # Background-process ambient state — the app's visibility into
  # `background_exec.rs`'s host-process registry (docs/issues.md
  # "Background shell + process management"). Kernel-derived from
  # `BackgroundRegistry::summary_by_context`, scoped to exactly the
  # processes THIS context started (same isolation the
  # `list_background_processes`/`read_background_output`/
  # `kill_background_process` MCP tools already enforce). This is the
  # aggregate ambient signal for a human glancing at the dock, not the
  # per-process detail those tools return.
  backgroundRunningCount @23 :UInt32;           # 0 = none running right now
  backgroundOldestRunningStartedAt @24 :UInt64; # Unix ms of the OLDEST still-running process; 0 = nothing running
  backgroundLastFinishedAt @25 :UInt64;         # Unix ms the most-recently-finished process ended; 0 = none finished yet
                                                 # (including one that finished >DEFAULT_RETENTION ago and was reaped —
                                                 # the registry's own forgetting is this field's natural TTL)
  backgroundLastFinishedStatus @26 :Text;       # "exited"/"killed" for the process backgroundLastFinishedAt describes;
                                                 # empty = none finished yet
  backgroundLastExitCode @27 :Int32 = -1;       # exit code for an "exited" finish; -1 sentinel = "killed" or none
                                                 # (real exit codes, incl. 128+signal, are never negative — see
                                                 # background_exec.rs's exit_code_from_status)

  # Track D (cast-on-context, 2026-08-03): the named model ensemble this
  # context plays under (`ContextRow::cast_id`, resolved to its label
  # kernel-side — the wire never ships a bare CastId for the app to look up
  # itself). Empty = no cast assigned (falls through to the registry default
  # at resolution time; see `kaijutsu-kernel`'s `model_resolution` module).
  castLabel @28 :Text;

  # Advisory hostname the registering client self-reported (`gethostname` on
  # the kaijutsu-mcp side today) — set ONCE at creation via
  # `setContextOriginHost`, never overwritten on a later resume/attach from a
  # different machine (mirrors `createdAt`/`createdBy`: an origin fact, not a
  # "last seen from" one). Empty = unknown: an old client, a creation path
  # with no client to ask (genesis bootstrap, fork), or a pre-migration row.
  # Shared-trust model — this is observability, not auth; the server trusts
  # the client's self-report (docs/issues.md "cc-* hook re-registration
  # mints a new context per MCP relaunch"). `kj context info`/`list` render
  # empty as "-".
  originHost @29 :Text;
}

struct PresetInfo {
  id @0 :Data;                    # 16-byte PresetId (UUIDv7)
  label @1 :Text;
  description @2 :Text;

  # Track D (2026-08-03): the `presets` table's `provider`/`model` columns
  # are demolished — a preset now narrows to a cast reference instead of
  # pinning a provider/model pair itself ("cast = who plays; preset = patch
  # recall over verb args"). capnp ordinals are dense and never renumbered,
  # so @3/@4 stay DECLARED but UNUSED (the server never sets them; always
  # empty on the wire) rather than removed — `castLabel` (resolved
  # kernel-side, same convention as `ContextHandleInfo.castLabel`) is the
  # additive replacement at @5. Empty = the preset moves no model knobs (the
  # three factory presets' shape).
  provider @3 :Text; # UNUSED since Track D — always empty; do not repurpose
  model @4 :Text;    # UNUSED since Track D — always empty; do not repurpose
  castLabel @5 :Text;
}

struct SimilarContext {
  contextId @0 :Data;   # 16-byte ContextId
  score @1 :Float32;    # Cosine similarity, clamped to [0.0, 1.0]
  label @2 :Text;       # Optional context label
}

struct ContextCluster {
  clusterId @0 :UInt32;
  contextIds @1 :List(Data);  # List of 16-byte ContextIds
  label @2 :Text;             # Kernel-synthesized cluster label (e.g. top shared keyword); empty if none
}

# Information about a staged drift (pending content transfer)
struct StagedDriftInfo {
  id @0 :UInt64;
  sourceCtx @1 :Data;             # 16-byte ContextId
  targetCtx @2 :Data;             # 16-byte ContextId
  content @3 :Text;
  sourceModel @4 :Text;
  driftKind @5 :DriftKind;
  createdAt @6 :UInt64;
}

# A staged drift that exceeded its retry budget or whose target dropped
# out of the drift router. Mirrors `kaijutsu_kernel::drift::StagedDrift`.
struct DeadLetter {
  id @0 :UInt64;
  sourceCtx @1 :Data;
  targetCtx @2 :Data;
  content @3 :Text;
  sourceModel @4 :Text;       # empty when not known
  hasSourceModel @5 :Bool;
  driftKind @6 :Text;
  createdAt @7 :UInt64;
  retryCount @8 :UInt32;
}

# ============================================================================
# Music and time
# ============================================================================
# Wire types for the render-cue / beat-sync musical timebase (docs/midi.md) and live track state.

# A render directive crossing the seam to an off-box sink (docs/midi.md
# "Render is a wire cue"; docs/pcm.md "How it converges"). Mirrors
# kaijutsu_audio::RenderCue: mime-keyed symbolic content (or a CAS hash the
# sink resolves), scheduled at receipt + lead. Generalizes the slice-3
# play-now audio directive — audio samples, clip records, MIDI, and ABC all
# ride this one struct and the sink dispatches on `mime`.
struct RenderCue {
  mime @0 :Text;          # dispatch key: audio/wav, audio/midi, text/vnd.abc, …
  leadNanos @1 :UInt64;   # relative schedule lead; sink fires at receipt+lead. 0 = now.
  union {
    inline @2 :Data;      # inline symbolic content / tiny sample bytes
    casHash @3 :Text;     # hex ContentHash — resolved from CAS at the sink
  }
  # Sender wallclock (ns since UNIX_EPOCH) at emission — mirrors BeatRef.epochNs
  # below (phase-align Slice 2). 0 = unstamped (an old peer, or a synthetic cue
  # with no meaningful emission instant, e.g. a RENDER_FLUSH directive). A sink
  # back-dates `leadNanos` by however old the cue reads on receipt, so per-cue
  # transfer latency can't walk the render out of phase with the click
  # (`docs/midi.md`).
  epochNs @4 :UInt64;
  # The beat coordinate this cue is placed at (1 tick == 1 beat — see
  # publishBeatSync's own doc comment), stamped by the kernel for a
  # track-committed cell (phase-align Slice 2). Unset for directive cues with
  # no track/beat association (transport flush, kj play's play-now, the
  # PREPARE_MIME cache-warm directive) — those always dispatch immediately
  # regardless of a sink's clock mode.
  onsetBeat @5 :Float64;
  hasOnsetBeat @6 :Bool;  # True if onsetBeat is set (distinguishes 0.0 from unset)
}

# A low-rate beat reference for a sink's continuous local timebase (docs/midi.md
# "The relative-lead timebase, analyzed"). Mirrors kaijutsu_audio::BeatRef: the
# fractional beat coordinate at emission plus the tempo, PLUS the sender's
# wallclock at emission (epochNs) — mirroring reportClockEstimate's epochNs
# (the reverse-direction ClockEstimate path this mirrors forward). The sink
# back-dates each reference to its own emission instant (BeatRef::backdated_at)
# rather than folding every buffered reference at one receipt-time now — a
# delivery flood (refs queued behind a turn's streamed output) would otherwise
# walk the phasor several beats. epochNs == 0 means unstamped (an old peer) —
# the sink falls back to receipt time, the old behavior. The phasor still never
# hard-resyncs: a fresh reference nudges it, it doesn't snap.
struct BeatRef {
  beat @0 :Float64;       # fractional beat coordinate at emission; integers are onsets
  tempoBps @1 :Float64;   # tempo in beats per second (120 BPM == 2.0)
  epochNs @2 :UInt64;     # sender wallclock (ns since UNIX_EPOCH) at emission; 0 = unstamped
}

# One MIDI port as a reporting sink sees it (docs/midi-next.md "Presence is
# sink-fed"). Backend-neutral by construction: `name` is the port's display
# name — what profile match strings match on — and `address` is the backend's
# own handle for it ("client:port" under ALSA, a unique id under CoreMIDI),
# opaque to the kernel and carried for logs/disambiguation only. No ALSA
# client numbers ever reach a profile.
struct MidiPortFact {
  name @0 :Text;
  address @1 :Text;
}

# Live state of one track (a clock domain: named cadence + score + attached
# contexts — docs/tracks.md). Answered from the beat scheduler's IN-MEMORY
# truth: the persisted `tracks` row's playhead lags (written only on transport
# transitions), so the wire never reads the row. Used by listTracks.
struct TrackInfo {
  id @0 :Text;                    # TrackId — the human name ([a-z0-9_-], never empty)
  scoreContextId @1 :Data;        # 16-byte ContextId of the track's score context
  playing @2 :Bool;               # transport: whether the clock is rolling
  playheadTick @3 :Int64;         # musical time (event-counted; freezes on pause)
  periodUs @4 :UInt64;            # beat period (the tempo knob), microseconds
  beatsPerPhrase @5 :UInt64;
  beatCount @6 :UInt64;           # beats elapsed while playing (resets on kernel restart)
  lastEpochNs @7 :UInt64;         # wall clock (ns) of the most recent beat; 0 = never fired
  clockKind @8 :Text;             # clock driver: "system" today, "modeled" at M3
  attached @9 :List(Data);        # 16-byte ContextIds currently bound to this track
}

# ============================================================================
# Kernel configuration
# ============================================================================
# Kernel identity, mount/consent policy, and timeout defaults.

struct KernelInfo {
  id @0 :Data;                    # 16-byte KernelId (UUIDv7)
  name @1 :Text;
  userCount @2 :UInt32;
  agentCount @3 :UInt32;
  contexts @4 :List(ContextHandleInfo);
}

struct KernelConfig {
  name @0 :Text;
  mounts @1 :List(MountSpec);
  consentMode @2 :ConsentMode;
}

# Kernel-wide timeout policy. Mirrors `kaijutsu_types::TimeoutPolicy`.
# Wire form is millis (UInt64); Rust side uses `Duration`.
struct TimeoutPolicy {
  kaishRequestTimeoutMs @0 :UInt64;
  rcScriptTimeoutMs @1 :UInt64;
  hookBodyTimeoutMs @2 :UInt64;
  initScriptTimeoutMs @3 :UInt64;
  llmRequestTimeoutMs @4 :UInt64;
  llmIdleTimeoutMs @5 :UInt64;
  mcpConnectTimeoutMs @6 :UInt64;
  mcpCallTimeoutDefaultMs @7 :UInt64;
}

# ============================================================================
# VFS and mounts
# ============================================================================
# The virtual filesystem's wire types and its RPC interface.

struct MountSpec {
  path @0 :Text;           # e.g. "/mnt/kaijutsu"
  source @1 :Text;         # e.g. "~/src/kaijutsu" or "kernel://other"
  writable @2 :Bool;
}

enum ConsentMode {
  collaborative @0;
  autonomous @1;
}

struct MountInfo {
  path @0 :Text;
  readOnly @1 :Bool;
}

enum FileType {
  file @0;
  directory @1;
  symlink @2;
}

struct FileAttr {
  size @0 :UInt64;
  kind @1 :FileType;
  perm @2 :UInt32;
  mtimeSecs @3 :UInt64;        # Seconds since UNIX epoch
  mtimeNanos @4 :UInt32;       # Nanoseconds
  nlink @5 :UInt32;
}

struct DirEntry {
  name @0 :Text;
  kind @1 :FileType;
}

struct SetAttr {
  size @0 :UInt64;             # 0 = not set (use hasSize)
  hasSize @1 :Bool;
  perm @2 :UInt32;
  hasPerm @3 :Bool;
  mtimeSecs @4 :UInt64;
  hasMtime @5 :Bool;
}

struct StatFs {
  blocks @0 :UInt64;
  bfree @1 :UInt64;
  bavail @2 :UInt64;
  files @3 :UInt64;
  ffree @4 :UInt64;
  bsize @5 :UInt32;
  namelen @6 :UInt32;
}

# One node of a recursive `Vfs.snapshot` walk — the FSN world's stage-0/1
# kernel plumbing (docs/scenes/vfs.md "Kernel plumbing: enumeration +
# fsnotify"). Self-recursive via `children`; capnp handles this natively
# (a List(SnapshotNode) is a pointer, no size-upfront problem).
struct SnapshotNode {
  name @0 :Text;
  kind @1 :FileType;
  size @2 :UInt64;
  mtimeSecs @3 :UInt64;        # Seconds since UNIX epoch
  childCount @4 :UInt32;       # Real child count, even when `children` is cut by depth/cap
  ignored @5 :Bool;            # gitignore CLASSIFICATION — metadata, never a filter; the
                                # world renders ignored entries, just differently (target/ is
                                # not skipped, it "fractures and glows its district")
  generation @6 :UInt64;       # Per-directory listing-generation stamp; 0 for files
  children @7 :List(SnapshotNode);
  truncatedHere @8 :Bool;      # This node's children were cut (depth or entry cap)
  denied @9 :Bool;             # The walk was REFUSED here (permission denied on the
                               # listing/attributes) — distinct from truncatedHere: a cut
                               # is the walker's budget, a denial is the filesystem saying
                               # no, rendered as a seam rather than failing the walk
}

struct VfsActivityEntry {
  # path is normalized MountTable form: absolute, /-rooted, no trailing slash —
  # the SAME namespace and format as Vfs.snapshot paths and the app's listing keys.
  path @0 :Text;
  total @1 :UInt64;      # ABSOLUTE monotonic activity total since kernel boot
  generation @2 :UInt64; # the dir's current listing-generation (stale-listing detection)
}

# Push channel for the activity digest stream: per-directory heat (content +
# structure mutations), delivered as absolute totals so a missed/dropped
# digest self-heals on the next tick (see kaijutsu-kernel's vfs/activity.rs
# for the full lossy-safe reasoning).
interface VfsActivityEvents {
  # Next free ordinal: 1. Ordinals are dense and permanent — never
  # reuse one, and never renumber outside a flag day; retiring a method
  # leaves a `retiredNN @NN ();` stub instead.

  # Lossy-safe: totals are absolute state snapshots, never deltas.
  onActivityDigest @0 (entries :List(VfsActivityEntry), globalTotal :UInt64);
}

interface Vfs {
  # Next free ordinal: 17. Ordinals are dense and permanent — never
  # reuse one, and never renumber outside a flag day; retiring a method
  # leaves a `retiredNN @NN ();` stub instead.

  # Reading
  getattr @0 (path :Text) -> (attr :FileAttr);
  readdir @1 (path :Text) -> (entries :List(DirEntry));
  read @2 (path :Text, offset :UInt64, size :UInt32) -> (data :Data);
  readlink @3 (path :Text) -> (target :Text);

  # Writing
  write @4 (path :Text, offset :UInt64, data :Data) -> (written :UInt32);
  create @5 (path :Text, mode :UInt32) -> (attr :FileAttr);
  mkdir @6 (path :Text, mode :UInt32) -> (attr :FileAttr);
  unlink @7 (path :Text);
  rmdir @8 (path :Text);
  rename @9 (from :Text, to :Text);
  truncate @10 (path :Text, size :UInt64);
  setattr @11 (path :Text, attr :SetAttr) -> (newAttr :FileAttr);
  symlink @12 (path :Text, target :Text) -> (attr :FileAttr);

  # Metadata
  readOnly @13 () -> (readOnly :Bool);
  statfs @14 () -> (stat :StatFs);

  # Path resolution
  realPath @15 (path :Text) -> (realPath :Text);

  # FSN world stage-0/1 plumbing (docs/scenes/vfs.md). Recursive listing walk
  # with generation stamps, VFS-mediated (never touches the host filesystem
  # directly) so nested mounts and virtual backends compose correctly.
  # `depth` (0 = just `root`, no children walked) and `maxEntries` (total
  # node count in the reply tree, root included) are both caller-supplied but
  # server-clamped to a hard ceiling regardless of what's asked — walking `/`
  # with no cap must be structurally impossible. `generation` mirrors
  # `root.generation` (a quick staleness check without descending the tree);
  # `truncated` is true iff ANY node in the reply has `truncatedHere` set (a
  # cheap "is this reply complete" check without walking the whole tree). A
  # backend error mid-walk fails the whole call rather than returning a
  # partial tree that would look indistinguishable from an intentional cut.
  snapshot @16 (path :Text, depth :UInt32, maxEntries :UInt32)
      -> (root :SnapshotNode, generation :UInt64, truncated :Bool);
}

# ============================================================================
# LLM
# ============================================================================
# Per-kernel multi-provider LLM request/response and configuration types.

struct LlmRequest {
  content @0 :Text;       # The prompt text
  model @1 :Text;         # Optional model name, uses server default if empty
  contextId @2 :Data;     # 16-byte ContextId — target context for response blocks
}

struct Completion {
  text @0 :Text;
  displayText @1 :Text;
  kind @2 :CompletionKind;
}

enum CompletionKind {
  command @0;
  path @1;
  variable @2;
  keyword @3;
}

# Information about a single LLM provider
struct LlmProviderInfo {
  name @0 :Text;              # Provider name (e.g., "anthropic", "gemini")
  defaultModel @1 :Text;      # Default model for this provider
  available @2 :Bool;         # Whether the provider is available (has API key)
  models @3 :List(Text);      # All available model IDs (from aliases + default)
}

# Current LLM configuration for a kernel
struct LlmConfigInfo {
  defaultProvider @0 :Text;   # Name of the default provider
  defaultModel @1 :Text;      # Name of the default model
  providers @2 :List(LlmProviderInfo);
}

# ============================================================================
# Tools
# ============================================================================
# Generic tool-call vocabulary — not MCP-specific; see the MCP section below for that protocol's own types.

struct ToolInfo {
  name @0 :Text;
  description @1 :Text;
}

struct ToolCall {
  tool @0 :Text;         # Tool name (e.g., "cell.edit")
  params @1 :Text;       # JSON parameters
  requestId @2 :Text;    # For correlation
}

struct ToolResult {
  requestId @0 :Text;
  success @1 :Bool;
  output @2 :Text;       # JSON result
  error @3 :Text;        # Error if !success
}

struct ToolSchema {
  name @0 :Text;
  description @1 :Text;
  inputSchema @2 :Text;  # JSON Schema for params
  category @3 :Text;
}

# Tool filter mode — controls which tools are available
struct ToolFilterConfig {
  union {
    all @0 :Void;                 # All tools available
    allowList @1 :List(Text);     # Only these tools available
    denyList @2 :List(Text);      # All except these tools available
  }
}

# ============================================================================
# Shell and output
# ============================================================================
# kaish structured output types and the KernelOutput push channel.

enum EntryType {
  text @0;
  file @1;
  directory @2;
  executable @3;
  symlink @4;
}

struct OutputNode {
  name @0 :Text;
  entryType @1 :EntryType;
  text @2 :Text;             # content when hasText=true; None vs Some("") distinction
  hasText @3 :Bool;
  cells @4 :List(Text);
  children @5 :List(OutputNode);
}

struct OutputData {
  headers @0 :List(Text);
  hasHeaders @1 :Bool;       # None vs Some([]) distinction
  root @2 :List(OutputNode);
  richJson @3 :Text;   # kj / builtin structured payload (JSON-encoded); empty = none. See OutputData::rich_json.
}

struct HistoryEntry {
  id @0 :UInt64;
  code @1 :Text;
  timestamp @2 :UInt64;
}

# Result from shell command execution (matches kaish ExecResult)
struct ShellExecResult {
  code @0 :Int64;           # Exit code (0 = success)
  ok @1 :Bool;              # Convenience: code == 0
  stdout @2 :Data;          # Raw bytes (may be binary — images, serialized blobs, etc.)
  stderr @3 :Text;          # Always text (diagnostics, warnings)
  data @4 :ShellValue;      # Parsed result value (structured)
  outputData @5 :OutputData; # Structured output for tables/trees
}

# Shell variable value (mirrors kaish ast::Value)
struct ShellValue {
  union {
    null @0 :Void;
    bool @1 :Bool;
    int @2 :Int64;
    float @3 :Float64;
    string @4 :Text;
    json @5 :Text;       # serde_json::Value serialized
    blob @6 :Text;       # LEGACY: kaish dropped Value::Blob in 0.9; never produced now
    bytes @7 :Data;      # inline binary (kaish Value::Bytes) — raw bytes on wire
  }
}

# Shell variable name + value pair
struct ShellVar {
  name @0 :Text;
  value @1 :ShellValue;
}

struct KernelOutputEvent {
  execId @0 :UInt64;
  event @1 :OutputEvent;
}

struct OutputEvent {
  union {
    stdout @0 :Text;
    stderr @1 :Text;
    exitCode @2 :Int32;
    structured @3 :OutputData;  # Structured output
  }
}

interface KernelOutput {
  # Next free ordinal: 1. Ordinals are dense and permanent — never
  # reuse one, and never renumber outside a flag day; retiring a method
  # leaves a `retiredNN @NN ();` stub instead.

  onOutput @0 (event :KernelOutputEvent);
}

# ============================================================================
# Peers
# ============================================================================
# Named RPC participants the kernel can dispatch invocations to (the app, MCP servers, future clients).

# Configuration for attaching a peer.
struct PeerConfig {
  nick @0 :Text;              # Stable address / role (e.g. "kaijutsu-app"); shared across windows.
  instance @1 :Text;          # Unique-per-process token (a UUID the peer mints once), so two
                              # windows of the same nick coexist. Empty → keyed by nick (back-compat).
}

# Information about an attached peer.
struct PeerInfo {
  nick @0 :Text;
  attachedAt @1 :UInt64;      # Unix timestamp ms
}

# Callback for receiving peer invocations (reverse RPC).
# Registered via attachPeer; the kernel calls back to dispatch work.
# Same pattern as MCP sampling: server holds callback to client.
interface PeerCommands {
  # Next free ordinal: 1. Ordinals are dense and permanent — never
  # reuse one, and never renumber outside a flag day; retiring a method
  # leaves a `retiredNN @NN ();` stub instead.

  invoke @0 (action :Text, params :Data) -> (result :Data);
}

# ============================================================================
# MCP (Model Context Protocol) Types
# ============================================================================

struct McpServerInfo {
  name @0 :Text;              # Server name
  protocolVersion @1 :Text;   # MCP protocol version
  serverName @2 :Text;        # Server's reported name
  serverVersion @3 :Text;     # Server's reported version
  tools @4 :List(McpToolInfo);
}

struct McpToolInfo {
  name @0 :Text;              # Tool name (e.g., "git_status")
  description @1 :Text;       # Tool description
  inputSchema @2 :Text;       # JSON Schema for parameters
}

struct McpToolCall {
  tool @0 :Text;              # Tool name (e.g., "git_status")
  arguments @1 :Text;         # JSON-encoded arguments
}

struct McpToolResult {
  content @0 :Text;           # Result content (text)
  isError @1 :Bool;           # True if the tool returned an error
}

struct McpResource {
  uri @0 :Text;               # Resource URI (e.g., "file:///path/to/file")
  name @1 :Text;              # Resource name
  description @2 :Text;       # Optional description
  mimeType @3 :Text;          # Optional MIME type
  hasDescription @4 :Bool;    # True if description is set
  hasMimeType @5 :Bool;       # True if mimeType is set
}

struct McpResourceContents {
  uri @0 :Text;               # Resource URI
  mimeType @1 :Text;          # MIME type of content
  hasMimeType @2 :Bool;       # True if mimeType is set
  union {
    text @3 :Text;            # Text content
    blob @4 :Data;            # Binary content (base64 on wire)
  }
}

# Callback interface for receiving MCP resource events from the server
interface ResourceEvents {
  # Next free ordinal: 2. Ordinals are dense and permanent — never
  # reuse one, and never renumber outside a flag day; retiring a method
  # leaves a `retiredNN @NN ();` stub instead.

  # Called when a resource's content is updated
  onResourceUpdated @0 (server :Text, uri :Text, contents :McpResourceContents, hasContents :Bool);
  # Called when a server's resource list changes (resources added or removed)
  onResourceListChanged @1 (server :Text, resources :List(McpResource), hasResources :Bool);
}

# MCP Root — workspace directory advertised to servers
struct McpRoot {
  uri @0 :Text;              # file:// URI for the root
  name @1 :Text;             # Optional display name
  hasName @2 :Bool;
}

# MCP Prompt — reusable prompt template from a server
struct McpPrompt {
  name @0 :Text;             # Prompt identifier
  title @1 :Text;            # Optional display title
  hasTitle @2 :Bool;
  description @3 :Text;      # Optional description
  hasDescription @4 :Bool;
  arguments @5 :List(McpPromptArgument);
}

struct McpPromptArgument {
  name @0 :Text;             # Argument name
  title @1 :Text;            # Optional title
  hasTitle @2 :Bool;
  description @3 :Text;      # Optional description
  hasDescription @4 :Bool;
  required @5 :Bool;         # Whether the argument is required
}

struct McpPromptMessage {
  role @0 :Text;             # "user" or "assistant"
  content @1 :Text;          # Message content
}

# MCP Progress — updates during long-running operations
struct McpProgress {
  server @0 :Text;           # Server name
  token @1 :Text;            # Progress token
  progress @2 :Float64;      # Current progress value
  total @3 :Float64;         # Total value if known
  hasTotal @4 :Bool;
  message @5 :Text;          # Human-readable message
  hasMessage @6 :Bool;
}

interface ProgressEvents {
  # Next free ordinal: 1. Ordinals are dense and permanent — never
  # reuse one, and never renumber outside a flag day; retiring a method
  # leaves a `retiredNN @NN ();` stub instead.

  onProgress @0 (progress :McpProgress);
}

# MCP Elicitation — server requests user input
struct McpElicitationRequest {
  requestId @0 :Text;        # Unique request ID
  server @1 :Text;           # Server name
  message @2 :Text;          # Message to display
  schema @3 :Text;           # JSON Schema for response validation
  hasSchema @4 :Bool;
}

struct McpElicitationResponse {
  action @0 :Text;           # "accept", "decline", or "cancel"
  content @1 :Text;          # JSON response data
  hasContent @2 :Bool;
}

interface ElicitationEvents {
  # Next free ordinal: 1. Ordinals are dense and permanent — never
  # reuse one, and never renumber outside a flag day; retiring a method
  # leaves a `retiredNN @NN ();` stub instead.

  onRequest @0 (request :McpElicitationRequest) -> (response :McpElicitationResponse);
}

# ============================================================================
# Approval-ledger change notification
# ============================================================================
# Carries a generation and nothing else — no ask id, no status, no content.
# Two properties depend on that emptiness:
#
# - Coalescing is lossless. N changes inside a delivery window collapse to
#   one notification bearing the highest generation, and a client that polls
#   afterwards still observes all N.
# - A dropped notification cannot corrupt anything; the worst case is a late
#   poll. The ledger is the authority, and answering is its claim/decide
#   transaction (exactly one answerer wins), reached through `kj ledger`.
#
# The generation is durable in the ledger's SQLite and maintained by
# triggers, so it cannot report a change that did not commit. The kernel
# jumps it ahead by a gap at boot, so a restart reads as a discontinuity
# rather than a silent resume. Int64 because SQLite's INTEGER is i64.

interface LedgerEvents {
  # Next free ordinal: 1. Ordinals are dense and permanent — never
  # reuse one, and never renumber outside a flag day; retiring a method
  # leaves a `retiredNN @NN ();` stub instead.

  onChanged @0 (generation :Int64) -> ();
}

# MCP Completion — argument value suggestions
struct McpCompletionResult {
  values @0 :List(Text);     # Suggested completions
  total @1 :UInt32;          # Total available if known
  hasTotal @2 :Bool;
}

# MCP Logging — log messages from servers
struct McpLogMessage {
  server @0 :Text;           # Server name
  level @1 :Text;            # Log level (error, warn, info, debug)
  logger @2 :Text;           # Logger name
  hasLogger @3 :Bool;
  data @4 :Text;             # JSON log data
}

interface LoggingEvents {
  # Next free ordinal: 1. Ordinals are dense and permanent — never
  # reuse one, and never renumber outside a flag day; retiring a method
  # leaves a `retiredNN @NN ();` stub instead.

  onLog @0 (log :McpLogMessage);
}

# ============================================================================
# kj
# ============================================================================
# Wire shape for the ACP-facing kj command catalog.

struct KjCommand {
  name @0 :Text;
  description @1 :Text;
  inputHint @2 :Text;
  argvPrefix @3 :List(Text);
}

# ============================================================================
# The kernel interface
# ============================================================================
# World and Kernel — the RPC surface everything above exists to serve; last because it is built from the vocabulary above it.

interface World {
  # Next free ordinal: 3. Ordinals are dense and permanent — never
  # reuse one, and never renumber outside a flag day; retiring a method
  # leaves a `retiredNN @NN ();` stub instead.

  # Identity
  whoami @0 () -> (identity :Identity);

  # Kernel management
  listKernels @1 () -> (kernels :List(KernelInfo));
  bindKernel @2 (trace :TraceContext) -> (kernel :Kernel, kernelId :Data);
}

interface Kernel {
  # Next free ordinal: 102. Ordinals are dense and permanent — never
  # reuse one, and never renumber outside a flag day; retiring a method
  # leaves a `retiredNN @NN ();` stub instead.

  # ==========================================================================
  # Kernel info & liveness
  # ==========================================================================

  getInfo @0 (trace :TraceContext) -> (info :KernelInfo);

  # Cheap liveness probe used by the client's reconnection FSM.
  # Returns the server-assigned kernel ID (so the client can detect a
  # silent rebind across kernel restart) and the server's wall-clock time
  # in milliseconds since the Unix epoch (for clock-skew diagnostics).
  # Handler must not take any per-context locks — this exists to detect
  # liveness, not to validate kernel state.
  ping @1 (trace :TraceContext) -> (kernelId :Data, serverTimeMs :UInt64);

  # ==========================================================================
  # kaish execution
  # ==========================================================================

  execute @2 (code :Text, trace :TraceContext) -> (execId :UInt64);

  interrupt @3 (execId :UInt64, trace :TraceContext);

  complete @4 (partial :Text, cursor :UInt32, trace :TraceContext) -> (completions :List(Completion));

  subscribeOutput @5 (callback :KernelOutput);

  getCommandHistory @6 (limit :UInt32, trace :TraceContext) -> (entries :List(HistoryEntry));

  # ==========================================================================
  # Shell (kaish REPL with block output)
  # ==========================================================================

  # Creates ToolCall (ToolKind::Shell) and ToolResult (ToolKind::Shell) blocks, streams output via BlockEvents
  shellExecute @7 (code :Text, contextId :Data, trace :TraceContext, userInitiated :Bool) -> (commandBlockId :BlockId);

  # Shell state (kaish working directory and last result)
  getCwd @8 () -> (path :Text);

  setCwd @9 (path :Text) -> (success :Bool, error :Text);

  getLastResult @10 () -> (result :ShellExecResult);

  # Shell variable introspection (kaish scope)
  getShellVar @11 (name :Text) -> (value :ShellValue, found :Bool);

  setShellVar @12 (name :Text, value :ShellValue) -> (success :Bool, error :Text);

  listShellVars @13 () -> (vars :List(ShellVar));

  # Addressed forms of the durable context cwd operations. Unlike getCwd /
  # setCwd these never consult or mutate the connection's ambient joined
  # context, so one client can safely manage several sessions.
  getContextCwd @97 (contextId :Data, trace :TraceContext) -> (path :Text, found :Bool);

  setContextCwd @98 (contextId :Data, path :Text, trace :TraceContext) -> (success :Bool, error :Text);

  # ==========================================================================
  # VFS & mounts
  # ==========================================================================

  vfs @14 () -> (vfs :Vfs);

  listMounts @15 () -> (mounts :List(MountInfo));

  mount @16 (path :Text, source :Text, writable :Bool);

  unmount @17 (path :Text) -> (success :Bool);

  # ── VFS activity digest stream (Lane K, FSN slice-1, docs/scenes/vfs.md) ──
  # Push channel: server ticks a per-connection timer and streams
  # `onActivityDigest` callbacks reporting which directories have gotten hot
  # since this subscriber's last delivered digest. `intervalMs` is the
  # requested tick period; 0 requests the server default (1000ms), and the
  # server floors any request below 500ms. A quiet tick (nothing changed
  # anywhere since last delivery) sends nothing at all. Connection-scoped,
  # same lifecycle as subscribeEditor — no cross-connection dedupe, the
  # bridge simply dies with the connection.
  subscribeVfsActivity @89 (callback :VfsActivityEvents, intervalMs :UInt32);

  # ==========================================================================
  # Tool execution
  # ==========================================================================

  executeTool @18 (call :ToolCall, trace :TraceContext) -> (result :ToolResult);

  getToolSchemas @19 (trace :TraceContext) -> (schemas :List(ToolSchema));

  # ==========================================================================
  # LLM operations & configuration (per-kernel multi-provider)
  # ==========================================================================

  prompt @20 (request :LlmRequest, trace :TraceContext) -> (promptId :Text);

  getLlmConfig @21 (trace :TraceContext) -> (config :LlmConfigInfo);

  setDefaultProvider @22 (provider :Text) -> (success :Bool, error :Text);

  setDefaultModel @23 (provider :Text, model :Text) -> (success :Bool, error :Text);

  # Configure LLM provider/model for a specific context (per-context model assignment).
  # If contextId is empty/missing, uses the connection's current context.
  configureLlm @24 (provider :Text, model :Text, trace :TraceContext, contextId :Data) -> (success :Bool, error :Text);

  # ==========================================================================
  # Context management & lifecycle (ContextId = 16-byte UUIDv7 as Data)
  # ==========================================================================

  listContexts @25 (trace :TraceContext) -> (contexts :List(ContextHandleInfo));

  createContext @26 (label :Text, contextType :Text) -> (id :Data);

  joinContext @27 (contextId :Data, instance :Text, trace :TraceContext) -> (contextId :Data);

  # Get this kernel's context ID and label
  getContextId @28 (trace :TraceContext) -> (id :Data, label :Text);

  # Rename a context's label
  renameContext @29 (contextId :Data, label :Text);

  # Set lifecycle state for a context (e.g., Staging → Live).
  setContextState @30 (contextId :Data, state :Text, trace :TraceContext) -> (success :Bool, error :Text);

  # Conclude a context: the explicit "this work is done" act (the kaijutsu
  # equivalent of exit-ing a mux window). Sets contextState = "concluded" and
  # stamps `concludedAt`. Distinct from a transient detach (contextLeave, which
  # must NOT demote) and from archive (which hides the context). Reversible by
  # forking/recovering from the concluded set — deliberately not first-class, so
  # there is no dedicated un-conclude verb. Idempotent: re-concluding a concluded
  # context succeeds without restamping. `success = false` with `error` set when
  # the context can't be concluded (unknown / already archived).
  conclude @31 (contextId :Data, trace :TraceContext) -> (success :Bool, error :Text);

  # Drop this session's binding to a context. Used by the constellation
  # archive flow so the drift router can mark the context Archived
  # without an active session re-resurrecting it on the next op.
  # Idempotent: returns true if the session held a binding to that
  # context, false otherwise.
  contextLeave @32 (contextId :Data, trace :TraceContext) -> (left :Bool);

  # List all presets for this kernel
  listPresets @33 (trace :TraceContext) -> (presets :List(PresetInfo));

  # ── Label resolution (DB-driven — deliberately NOT `listContexts`) ──

  # Resolve a label straight against KernelDb::find_context_by_label, bypassing
  # the in-memory DriftRouter entirely. `listContexts` reads the DriftRouter,
  # which `create_shared_kernel`'s boot-time recovery already re-registers
  # every NON-ARCHIVED context into on every restart (KernelDb is the
  # recovery source) — so for the common live/concluded case, `listContexts`
  # and this call agree. The gap is narrower than "never rehydrated": an
  # ARCHIVED context's DriftRouter entry does NOT survive a restart (recovery
  # filters `archived_at IS NULL`), so it stays durable (KernelDb row +
  # BlockStore document both survive) but invisible to listContexts until
  # something re-registers it (see joinContext's registry-heal). More to the
  # point for registerSession's upsert/attach decision: even WITHOUT any
  # restart, a label a live context already holds needs resolving here rather
  # than through listContexts, because the decision (attach vs. suffix vs.
  # create) has to be made before creating anything — reusing listContexts
  # would mean scanning + label-matching every context in the kernel on every
  # register_session call instead of one indexed lookup. `found = false`
  # means no context currently holds this label.
  resolveContextLabel @92 (label :Text, trace :TraceContext) -> (found :Bool, info :ContextHandleInfo);

  # Set (or clear, on empty `originHost`) a context's advisory `originHost` —
  # see `ContextHandleInfo.originHost`'s doc comment. Called once, right
  # after `createContext`, by a client that knows its own hostname
  # (`register_session` on the kaijutsu-mcp side); NOT part of `createContext`
  # itself, so every other creation path (genesis bootstrap, fork, the app)
  # is unaffected and keeps shipping today's two-arg call. Shared-trust
  # model: advisory metadata, not auth — the server trusts the client's
  # self-report, and a failed call here is non-fatal to the caller's own
  # registration (log-and-continue, never a hard failure over a hostname).
  setContextOriginHost @94 (contextId :Data, originHost :Text, trace :TraceContext) -> (success :Bool, error :Text);

  # ── Time-well ring placement (docs/timewell.md) ──

  # Promote a context into the hand-picked ring 0 ("active"). Sets
  # `promotedAt` (first time only — never restamped on re-promote, so ring-0
  # order stays append-stable) and clears `demotedAt`. Promoting an ARCHIVED
  # context resurrects it: `archivedAt` cleared, a FRESH `promotedAt` stamped
  # (the old seat was surrendered at archive time), `concludedAt` untouched —
  # promote is the resurrection door; the archive is memory to drift back
  # from, not trash. Works from any state, not latched, not capability-gated
  # (same as `conclude`).
  promoteContext @83 (contextId :Data, trace :TraceContext) -> (success :Bool, error :Text);

  # Push a context outward one step on the demote ladder (kernel-owned
  # policy): promoted → clears `promotedAt` (back to automatic placement);
  # neither promoted nor demoted → sets `demotedAt`; already demoted →
  # archives it (single context, no subtree recursion, no latch). Not
  # capability-gated.
  demoteContext @84 (contextId :Data, trace :TraceContext) -> (success :Bool, error :Text);

  # Set or clear a context's `pausedAt` — DESIGN-ONLY for now: persisted and
  # exposed on the wire, but not yet wired to any behavioral gating (no
  # scheduler/turn-start changes). A paused context will eventually receive
  # no beat/OODA wakeups and reject turn-starts loudly; that gating is
  # deferred.
  setContextPaused @85 (contextId :Data, paused :Bool, trace :TraceContext) -> (success :Bool, error :Text);

  # Archive a single context — the well's single-keystroke archive action.
  # Unlike the `kj context archive` builtin (latched, recurses into
  # structural children), this is a single context, not latched, no subtree
  # recursion. Idempotent: archiving an already-archived context succeeds
  # without restamping (the underlying `archived_at IS NULL` guard is
  # already a no-op).
  archiveContext @86 (contextId :Data, trace :TraceContext) -> (success :Bool, error :Text);

  # ── Per-client durable view state (docs/shared-state.md "Retiring KV") ──
  # The kernel-managed replacement for the app's one production KV use:
  # "reopen the context this window was last looking at" on reconnect. Backed
  # by a small normalized `client_views` KernelDb row keyed by the app's
  # stable per-installation client id — not a stringly KV namespace.

  # Record the last-viewed context for `clientId`. Idempotent re-write on the
  # same value is harmless (the app fires this on every context switch,
  # including the restore itself).
  setLastContext @80 (clientId :Text, contextId :Text);

  # Read back the last-viewed context for `clientId`. `found = false` when no
  # view is on record for this client (matches `kvGet`/`getShellVar`'s
  # value+found shape rather than an empty-string sentinel).
  getClientView @81 (clientId :Text) -> (contextId :Text, found :Bool);

  # ==========================================================================
  # Blocks: queries
  # ==========================================================================

  # Fetch blocks by query: all, byIds, or byFilter.
  #
  # `version` is the context's mutation counter **at the instant the blocks
  # were read** — both come from one guard, so no mutation can slip between
  # them. It is what joins a snapshot to the change feed: a client subscribes,
  # fetches here, then discards every buffered delivery at or below this
  # version and applies the rest (docs/change-feed.md rules 21-26). Without it
  # a client cannot tell whether a buffered append is already included, and
  # applying one twice corrupts the text.
  getBlocks @34 (contextId :Data, query :BlockQuery, trace :TraceContext) -> (blocks :List(BlockSnapshot), version :UInt64);

  # Projected revision of a context's block document, with no oplog bytes:
  # clients use it for staleness/gap detection without decoding DTE.
  getContextVersion @35 (contextId :Data, trace :TraceContext) -> (version :UInt64);

  # Compact a context's oplog, bumping sync generation. Server-side
  # maintenance only (docs/change-feed.md "Deleted by this change") — a
  # client holding materialized block state (via `ContextObserver` /
  # `getBlocks`) is unaffected and is not notified.
  compactContext @36 (contextId :Data, trace :TraceContext) -> (newSize :UInt64, generation :UInt64);

  # ==========================================================================
  # Blocks: subscriptions
  # ==========================================================================

  subscribeBlocks @37 (callback :BlockEvents);

  # Subscribe to block events with server-side filtering.
  # Like subscribeBlocks but the server applies the filter before sending,
  # reducing bandwidth and client CPU during high-throughput streaming.
  subscribeBlocksFiltered @38 (callback :BlockEvents, filter :BlockEventFilter, instance :Text);

  # Subscribe to ONE context's change feed (docs/change-feed.md). The
  # replacement for the two methods above, and for the raw-operation events
  # they carry; they remain only until the in-repo clients migrate.
  #
  # Ordering with the snapshot matters and is the client's job: subscribe
  # FIRST, buffer what arrives, then `getBlocks` and read the version it
  # returns, discard every buffered delivery at or below that version, and
  # apply the rest in order. Fetching first loses an event that lands in the
  # gap; applying first corrupts the text; the version resolves the overlap.
  #
  # One subscription is one context, because the version is one context's
  # counter. Watching several means several subscriptions. The feed ends when
  # the observer capability is dropped — there is no unsubscribe call.
  subscribeContext @39 (contextId :Data, observer :ContextObserver, trace :TraceContext);

  # ==========================================================================
  # Blocks: mutation & curation
  # ==========================================================================

  # Set the excluded flag on a block (staging curation).
  setBlockExcluded @40 (contextId :Data, blockId :BlockId, excluded :Bool, trace :TraceContext) -> (ackVersion :UInt64);

  # Move a block to a new position. When `hasAfter` is true, `after` is
  # the block to land after; otherwise the block is parked at the
  # beginning of the document. Mirrors the `has_parent_id` idiom used
  # elsewhere in the schema.
  moveBlock @41 (contextId :Data, blockId :BlockId, hasAfter :Bool, after :BlockId, trace :TraceContext) -> (ackVersion :UInt64);

  # ==========================================================================
  # Blocks: timeline & lineage
  # ==========================================================================

  # The past is read-only, but forking is ubiquitous.

  # Cherry-pick a block into another context (carries lineage)
  cherryPickBlock @47 (sourceBlockId :BlockId, targetContextId :Data, trace :TraceContext) -> (newBlockId :BlockId);

  # Get context version history for timeline scrubber
  getContextHistory @48 (contextId :Data, limit :UInt32, trace :TraceContext) -> (snapshots :List(VersionSnapshot));

  # ── RPC authoring (docs/crdt-position-2026-08.md, migration step 3) ───────
  #
  # The verbs that let a client author blocks WITHOUT being a storage replica.
  # `kaijutsu-mcp` was the last client that still pushed ops; these verbs
  # replaced that path, and the 2026-08-15 flag day then retired `pushOps`
  # (@37, zero production callers) outright — the Option-2 client contract
  # ("consume a projected stream, author via RPC") is real rather than
  # aspirational, and concurrent merge into kernel documents is now
  # structurally impossible rather than merely unused.
  #
  # Why not the existing `block_create` MCP tool: it hardcodes `after`,
  # `Status::Done` and `ContentType::Plain`, and its `metadata` argument is
  # parsed and then never read — so tool blocks routed through it arrive with
  # no name and no input, and NOTHING errors. That silent-drop is exactly
  # what these verbs exist to avoid, which is why every tool field below is
  # a first-class parameter.
  #
  # Shape follows Amy's ruling (2026-08-13): a tool call and its result take
  # "a quick lock at startup, then run independently … so the async result
  # can flow when it's ready." So authoring is a RESERVATION, not a
  # transaction. `authorBlock` is short and returns an id; the result arrives
  # later as its own `authorBlock` (parented to the call) plus a
  # `completeBlock` on the call. Concurrent tool calls share nothing but their
  # own ids, and a ToolCall sitting at `running` is a legitimate pending state
  # — the tool really is still running — not an orphan. Serialization, where a
  # tool needs it, is that tool's choice rather than a property baked in here.

  # Author one block. `principalId` is supplied by the caller: shared-trust
  # model, same as `setContextOriginHost`'s self-reported hostname, and the
  # kernel-side `insert_block_as` this lands on already takes an explicit
  # principal. Callers pass their agent-session principal
  # (`PrincipalId::for_agent_session`) so a block names the agent that caused
  # it and keeps naming it across process restarts.
  #
  # Empty `Data`/`Text` means absent: no `parentId` = root, no `afterId` =
  # append, no `toolName`/`toolInput` = not a tool block. `contentType` is the
  # MIME hint (empty = heuristic detection), matching `BlockSnapshot`.
  authorBlock @95 (
    contextId :Data,
    principalId :Data,
    role :Role,
    kind :BlockKind,
    status :Status,
    content :Text,
    contentType :Text,
    parentId :BlockId,
    hasParentId :Bool,
    afterId :BlockId,
    hasAfterId :Bool,
    toolName :Text,
    toolInput :Text,
    toolKind :ToolKind,
    hasToolKind :Bool,
    trace :TraceContext
  ) -> (blockId :BlockId, error :Text);

  # Move an already-authored block to a terminal state once its work
  # finishes. The other half of reserve-then-flow: whoever reserved the block
  # calls this whenever the result is ready, with no lock held in between and
  # no ordering relationship to any other tool call in flight.
  completeBlock @96 (
    contextId :Data,
    blockId :BlockId,
    status :Status,
    isError :Bool,
    exitCode :Int32,
    hasExitCode :Bool,
    trace :TraceContext
  ) -> (success :Bool, error :Text);

  # ==========================================================================
  # Turn control
  # ==========================================================================

  # Interrupt a running LLM stream or shell jobs for a context.
  # immediate=false → soft interrupt (stop agentic loop after current tool turn).
  # immediate=true  → hard interrupt (abort LLM stream + kill all kaish jobs).
  # Returns success=false when context has no active interrupt state (i.e. nothing running).
  interruptContext @42 (contextId :Data, immediate :Bool, trace :TraceContext) -> (success :Bool);

  # Push channel: the server bridges the kernel's `TurnFlow` outcome bus onto
  # `TurnEvents` callbacks. Every turn announces — interactive prompts and
  # kernel-driven autonomous turns alike — carrying the structured stop reason
  # (`end_turn` / cancelled / `max_tokens` / `max_iterations`) that a client
  # cannot infer from block status at all.
  #
  # Kernel-wide, following `subscribeEditor`: no per-context filter parameter,
  # because the event names its own `contextId` and a client watching several
  # contexts should not need several subscriptions. Connection-scoped — the
  # bridge dies with the connection, no cross-connection dedupe.
  subscribeTurnEvents @91 (callback :TurnEvents);

  # ==========================================================================
  # Input document (compose draft per context)
  # ==========================================================================

  # Each context has a companion draft block for compose input — an ordinary
  # block at `Status::Draft`, one per (context, principal). Any participant
  # can read/write it. Submit snapshots it to a conversation block.

  # High-level edit: insert text at position, delete characters
  editInput @43 (contextId :Data, pos :UInt64, insert :Text, delete :UInt64, trace :TraceContext) -> (ackVersion :UInt64);

  # Full state fetch for join/reconnect recovery
  getInputState @44 (contextId :Data, trace :TraceContext) -> (content :Text, ops :Data, version :UInt64);

  # Atomic submit: read input, create block, clear input.
  # Mode is explicit — no prefix detection.
  submitInput @45 (contextId :Data, mode :InputMode, trace :TraceContext) -> (commandBlockId :BlockId);

  # Clear the input document for a context (discard draft).
  # Emits InputCleared so all clients can reset their compose state.
  clearInput @46 (contextId :Data, trace :TraceContext) -> ();

  # ==========================================================================
  # Semantic search & clustering
  # ==========================================================================

  # Semantic search: find contexts similar to a text query
  searchSimilar @49 (query :Text, k :UInt32, trace :TraceContext) -> (results :List(SimilarContext));

  # Context neighbors: find contexts similar to a given context
  getNeighbors @50 (contextId :Data, k :UInt32, trace :TraceContext) -> (results :List(SimilarContext));

  # Clustering: group contexts by semantic similarity
  getClusters @51 (minClusterSize :UInt32, trace :TraceContext) -> (clusters :List(ContextCluster));

  # ==========================================================================
  # Drift (cross-context communication with provenance)
  # ==========================================================================

  # List staged drifts pending flush
  driftQueue @52 () -> (staged :List(StagedDriftInfo));

  # Cancel a staged drift by ID
  driftCancel @53 (stagedId :UInt64) -> (success :Bool);

  # Inspect the drift router's dead-letter queue. Non-consuming — pairs with
  # replayDeadLetter. Drifts land here when MAX_DRIFT_RETRIES is exceeded or
  # the target context unregisters before delivery.
  listDeadLetters @54 (trace :TraceContext) -> (items :List(DeadLetter));

  # Replay a dead-letter item by id: extract from DLQ, reset retry count,
  # push back to the staging queue for another flush attempt. Returns
  # `replayed = false` when the id isn't present (already drained or
  # never queued).
  replayDeadLetter @55 (id :UInt64, trace :TraceContext) -> (replayed :Bool);

  # ==========================================================================
  # Configuration (config as kernel documents)
  # ==========================================================================

  # Config files (theme.toml, bindings.toml, mcp.toml, etc.) are kernel
  # documents — the kernel is their sole owner, and there is no host file.

  # Get config content
  getConfig @56 (path :Text) -> (content :Text, error :Text);

  # ==========================================================================
  # MCP (Model Context Protocol) management
  # ==========================================================================

  listMcpServers @57 () -> (servers :List(McpServerInfo));

  callMcpTool @58 (call :McpToolCall, trace :TraceContext) -> (result :McpToolResult);

  # MCP Resources (push-first with caching)
  listMcpResources @59 (server :Text, trace :TraceContext) -> (resources :List(McpResource));

  subscribeMcpResources @60 (callback :ResourceEvents, instance :Text);

  # MCP Roots (client advertises workspaces to servers)
  setMcpRoots @61 (roots :List(McpRoot));

  # MCP Prompts (poll-based with optional caching)
  listMcpPrompts @62 (server :Text) -> (prompts :List(McpPrompt));

  getMcpPrompt @63 (server :Text, name :Text, arguments :Text) -> (messages :List(McpPromptMessage));

  # MCP Progress (push-based streaming)
  subscribeMcpProgress @64 (callback :ProgressEvents);

  # MCP Elicitation (server-initiated requests for user input)
  subscribeMcpElicitations @65 (callback :ElicitationEvents, instance :Text);

  # MCP Completion (request/response)
  completeMcp @66 (server :Text, refType :Text, refName :Text, argName :Text, value :Text) -> (result :McpCompletionResult);

  # MCP Logging
  setMcpLogLevel @67 (server :Text, level :Text);

  subscribeMcpLogs @68 (callback :LoggingEvents);

  # MCP Cancellation
  cancelMcpRequest @69 (server :Text, requestId :Text);

  # ==========================================================================
  # Peer registry (drift navigation transport)
  # ==========================================================================

  # Peers are named RPC participants (the Bevy app, MCP servers, future
  # clients) that the kernel can dispatch invocations to. The transport is
  # how MCP-driven agents tell the app to switch contexts, etc.

  # Attach a peer to this kernel.
  # `commands` is the callback the kernel uses to invoke this peer.
  attachPeer @70 (config :PeerConfig, commands :PeerCommands) -> (info :PeerInfo);

  # List all attached peers on this kernel.
  listPeers @71 () -> (peers :List(PeerInfo));

  # Detach a peer from this kernel.
  detachPeer @72 (nick :Text);

  # Invoke a peer. Params and result are JSON bytes.
  invokePeer @73 (nick :Text, action :Text, params :Data) -> (result :Data);

  # ==========================================================================
  # In-app editor sessions (the vi/edit builtin; see docs/vi.md)
  # ==========================================================================

  # A kernel-owned vi session bound to the block that owns a path's text.
  # Session ids are global (no contextId needed); the path resolves the owner.
  # Renderers draw `editorState`/`subscribeEditor` and forward keys to
  # `editorKeys`. Edits mirror onto the block — rc/config permission errors
  # surface here loudly (crash over corruption).

  editorOpen @74 (path :Text, trace :TraceContext) -> (state :EditorState);
  editorKeys @75 (sessionId :UInt64, keys :Text, trace :TraceContext) -> (state :EditorState);
  editorState @76 (sessionId :UInt64, trace :TraceContext) -> (state :EditorState);
  editorSave @77 (sessionId :UInt64, trace :TraceContext) -> (state :EditorState);
  editorQuit @78 (sessionId :UInt64, trace :TraceContext) -> ();

  # Push channel: server streams editor state changes (incl. future remote merges).
  subscribeEditor @79 (callback :EditorEvents);

  # ==========================================================================
  # Transport and time (tracks, MIDI capture/presence, clock estimates)
  # ==========================================================================

  # ── Tracks (docs/tracks.md; the time well's Stage 3 wire slice) ──

  # Enumerate every track's live state, straight from the beat scheduler's
  # in-memory TrackState (see TrackInfo). Empty when no tracks exist or no
  # scheduler is wired (embedded/test kernels).
  listTracks @82 (trace :TraceContext) -> (tracks :List(TrackInfo));

  # Commit a captured-MIDI batch onto the track `contextId` is attached to
  # (docs/midi.md M2, "the ear is the sink's twin" — the structural reverse
  # of RenderCue: the app hears, collates on its musical timer, and pushes;
  # the kernel quantizes to the track grid and lands one data-only block in
  # the score context). `mime` must be the MIDI_CAPTURE_MIME batch record.
  # `casHash` "" = `payload` carries the record inline (the only form built
  # today; a non-empty casHash is the reserved CAS arm, mirroring
  # RenderCue's union — rejected until the client→kernel CAS write surface
  # lands, docs/issues.md).
  commitCapture @87 (contextId :Data, mime :Text, payload :Data, casHash :Text, trace :TraceContext) -> (blockId :BlockId);

  # One clock reference from an edge observer (docs/midi.md M3, "distribute
  # tempo, not pulses" — the reverse of the BeatSync push, third mirror in
  # the set): the observed master's beat coordinate + tempo, stamped with
  # the observer's wallclock. ~2 Hz fire-and-forget; the kernel resolves the
  # sender's track and slaves it only when that track's clock is `modeled`
  # (`kj transport clock <track> modeled`). `source` is the observed ALSA
  # port ("client:port"), for logs/attribution.
  reportClockEstimate @88 (contextId :Data, beat :Float64, tempoBps :Float64, epochNs :UInt64, source :Text, trace :TraceContext) -> ();

  # ── Sink-fed MIDI presence (docs/midi-next.md "Presence is sink-fed") ──

  # The app matches, the kernel records. A sink watches platform hotplug,
  # matches port facts against the backend-neutral match strings in
  # /etc/midi/devices/<name>, and reports here; the kernel writes the report
  # into its EPHEMERAL /run/midi/<device> store as provenance-tagged facts
  # (source: "sink"). Not context-scoped: presence is a fact about the rig,
  # not about a conversation. `present` false is a first-class report (unplug)
  # — stale presence that lies is worse than none, so the sink says so rather
  # than going quiet. `backend` is the reporting sink's platform backend
  # ("alsa", "coremidi"); `epochNs` is its wallclock at observation, and a
  # report older than the one on file is dropped (latest timestamp wins).
  # No facade gate, on purpose (the reportClockEstimate stance): presence is
  # inert sensor data, and every player is inside the trust boundary.
  # `sinkHost` is the reporting sink's own name for its machine — display and
  # provenance only, so `kj midi list` can answer "live, but WHERE?" across a
  # rig. It is NEVER the key anything is reaped by: presence is bound to the
  # *connection* the report arrived on (the kernel attaches its own
  # per-connection id and drops every record attributed to a connection when
  # it dies), because a sink that crashes cannot send an unplug and cannot be
  # trusted to identify itself honestly either. Empty from an older peer.
  reportMidiPresence @90 (device :Text, present :Bool, backend :Text, ports :List(MidiPortFact), epochNs :UInt64, trace :TraceContext, sinkHost :Text) -> ();

  # Retired: HookAction::Ask now runs through the approval ledger
  # (docs/gate-and-shell-split.md) and announces via subscribeLedgerEvents.
  retired93 @93 ();

  # Kernel-wide: a ledger change can originate from any call path (a gated
  # `shell_write` in one context, a `kj ledger` answer typed in a shell, a
  # rule learned by a sibling), so one subscription serves every context.
  # No per-context filter parameter — the notification carries no context
  # to filter on.
  #
  # Delivery coalesces server-side within the change feed's latency budget
  # (`kaijutsu-server`'s `context_feed::FEED_BATCH_WINDOW`), which is sound
  # because collapsing generations loses nothing.
  subscribeLedgerEvents @101 (callback :LedgerEvents);

  # ==========================================================================
  # kj (context-addressed command execution)
  # ==========================================================================

  # Run one context-addressed kj invocation through the materialized kaish
  # builtin. argv excludes the leading "kj". Keeping argv structured prevents
  # command text from becoming an unintended general shell surface. The latch
  # fields reserve the confirmation round trip even though the first ACP
  # catalog is deliberately read-mostly.
  executeKj @99 (contextId :Data, argv :List(Text), trace :TraceContext) -> (
    exitCode :Int32,
    stdout :Text,
    stderr :Text,
    commandBlockId :BlockId,
    latchCommand :Text,
    latchTarget :Text,
    latchMessage :Text,
    hasLatch :Bool,
    # `KjResult`'s structured data, JSON-encoded — empty when the verb
    # produced none. ALWAYS wired when available: no `--json` flag, no
    # opt-in parameter (Amy, 2026-08-17: "executeKj should always wire up
    # data even without explicit --json on the pipeline. why not?"). The
    # RPC therefore does not grow the `kj` surface at all while keeping the
    # typing intact across the wire, and a client can `jq` it the same way
    # kaish already can.
    #
    # It was already being computed and persisted onto the *output* block
    # (`block_output_data` → `set_output`), reachable only by following the
    # change feed and correlating — and `commandBlockId`, not the output
    # block's id, is what this method returns. Carrying it here is a return
    # path for a value the call already produced.
    #
    # Some `kj` verbs do not populate `.data` yet; filling those in is
    # ordinary follow-up work, not a blocker for this field.
    data :Text
  );

  # ACP-facing command metadata. argvPrefix is the exact kj argv represented
  # by name; clients append parsed user arguments. The server owns curation so
  # every frontend observes the same loadout-aware surface.
  getKjCommandCatalog @100 (contextId :Data, trace :TraceContext) -> (commands :List(KjCommand));
}
