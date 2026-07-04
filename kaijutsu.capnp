@0xb8e3f4a9c2d1e0f7;

# Kaijutsu RPC Schema
# Cap'n Proto interface for kernel-based collaborative spaces

# ============================================================================
# Distributed Tracing (W3C Trace Context)
# ============================================================================

# W3C Trace Context for distributed tracing across SSH boundary.
# See: https://www.w3.org/TR/trace-context/
struct TraceContext {
  traceparent @0 :Text;   # e.g. "00-4bf92f3577b6a8141fea3e3b0aa7d3f5-00f067aa0ba902b7-01"
  tracestate @1 :Text;    # vendor-specific key=value pairs (optional, may be empty)
}

# ============================================================================
# Identity
# ============================================================================

struct Identity {
  username @0 :Text;
  displayName @1 :Text;
  principalId @2 :Data;   # 16-byte PrincipalId (UUIDv7)
}

# ============================================================================
# Block-Based CRDT Types (DAG Architecture)
# ============================================================================
# Blocks are the fundamental primitive. Everything is blocks.
# BlockSnapshot is the unit of replication and display.

# Block identifier — globally unique across all contexts and agents.
# Uses typed UUIDs (binary, 16 bytes each) instead of strings.
struct BlockId {
  contextId @0 :Data;   # 16-byte ContextId (UUIDv7)
  principalId @1 :Data;     # 16-byte PrincipalId (UUIDv7)
  seq @2 :UInt64;
}

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

# ============================================================================
# Block Payloads (kind-specific structured fields on BlockSnapshot)
# ============================================================================

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
}

# Full context state — blocks + CRDT oplog for sync
struct ContextState {
  contextId @0 :Data;   # 16-byte ContextId (UUIDv7)
  blocks @1 :List(BlockSnapshot);
  version @2 :UInt64;
  ops @3 :Data;         # Full oplog bytes for CRDT sync
}

# ============================================================================
# Block Events & Subscriptions
# ============================================================================

# Discriminant for BlockFlow event variants (for subscription filtering).
enum BlockFlowKind {
  inserted @0;
  textOps @1;
  deleted @2;
  statusChanged @3;
  collapsedChanged @4;
  moved @5;
  syncReset @6;
  outputChanged @7;
  metadataChanged @8;
  contextSwitched @9;
  excludedChanged @10;
  # A render directive, not a block-level event — never meaningfully
  # filterable by context/kind (see `BlockFlow::matches_filter`'s unconditional
  # bypass for `RenderCue`). Kept 1:1 with the Rust `BlockFlowKind` enum anyway
  # so the two never drift silently out of sync.
  renderCue @11;
  # A low-rate beat reference for the sink's continuous timebase (the metronome
  # phasor, docs/midi.md "The relative-lead timebase, analyzed"). Also a
  # directive — bypasses `matches_filter` like `renderCue`.
  beatSync @12;
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
}

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
}

# A low-rate beat reference for a sink's continuous local timebase (docs/midi.md
# "The relative-lead timebase, analyzed"). Mirrors kaijutsu_audio::BeatRef: the
# fractional beat coordinate at emission plus the tempo. Carries no absolute
# instant (a process-local one can't cross the wire) — the sink stamps receipt
# locally, exactly as it re-anchors RenderCue.leadNanos. The phasor slews toward
# it; it never hard-resyncs.
struct BeatRef {
  beat @0 :Float64;       # fractional beat coordinate at emission; integers are onsets
  tempoBps @1 :Float64;   # tempo in beats per second (120 BPM == 2.0)
}

# Callback for receiving block updates from server
interface BlockEvents {
  onBlockInserted @0 (contextId :Data, block :BlockSnapshot, afterId :BlockId, hasAfterId :Bool, ops :Data);
  onBlockDeleted @1 (contextId :Data, blockId :BlockId);
  onBlockCollapsed @2 (contextId :Data, blockId :BlockId, collapsed :Bool);
  onBlockMoved @3 (contextId :Data, blockId :BlockId, afterId :BlockId, hasAfterId :Bool);
  onBlockStatusChanged @4 (contextId :Data, blockId :BlockId, status :Status);
  # `seqNum` is a per-context monotonic counter (M2-B2). Clients use it
  # to detect dropped events when the broadcast channel overflows; on a
  # gap, re-fetch via `getContextSync` / `getInputState`.
  onBlockTextOps @5 (contextId :Data, blockId :BlockId, ops :Data, seqNum :UInt64);
  onSyncReset @6 (contextId :Data, generation :UInt64);

  # Input document events (compose scratchpad)
  onInputTextOps @7 (contextId :Data, ops :Data, seqNum :UInt64);
  onInputCleared @8 (contextId :Data);

  # Session control events (server → client)
  onContextSwitched @9 (contextId :Data);

  # Block excluded flag changed (staging curation)
  onBlockExcludedChanged @10 (contextId :Data, blockId :BlockId, excluded :Bool);

  # Scalar metadata changed (exit code, stderr, content type, …). Applied
  # directly to the client store — frontier-independent, so it survives a
  # reconnect even when text ops are gated behind a full resync.
  onBlockMetadataChanged @11 (contextId :Data, blockId :BlockId, metadata :BlockMetadata);

  # Structured output data changed (output is not DTE-tracked, so it rides
  # its own event rather than the block text op stream).
  onBlockOutputChanged @12 (contextId :Data, blockId :BlockId, output :OutputData);

  # Render a cue (docs/pcm.md, docs/midi.md "Render is a wire cue"). A kernel
  # directive (from `kj play`, later the track render seam), not a block change
  # — carried on this channel because it already fans out to every attached
  # client. `contextId` is the originating context (reserved for future
  # per-listener routing); the standalone slice forwards unconditionally.
  onRenderCue @13 (contextId :Data, cue :RenderCue);

  # A beat reference for the sink's continuous timebase (docs/midi.md "The
  # relative-lead timebase, analyzed"). Emitted at a low rate while a track's
  # clock rolls; the sink's phasor extrapolates the beats between and slews
  # toward each reference. Like onRenderCue, a directive that fans out to every
  # attached client. `contextId` is the track's score context (the same key
  # onRenderCue uses), so a sink can associate a beat with its track.
  onBeatSync @14 (contextId :Data, beatRef :BeatRef);
}

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
# The push channel exists so a peer's CRDT merge into an open block reaches
# every renderer the instant it lands — collaborative editing, not poll lag.
interface EditorEvents {
  onEditorState @0 (state :EditorState);
  onEditorClosed @1 (sessionId :UInt64);
}

# ============================================================================
# Block Queries & Timeline
# ============================================================================

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
  excludeCompacted @6 :Bool;
  limit @7 :UInt32;
  maxDepth @8 :UInt32;
  parentId @9 :BlockId;
  hasParentId @10 :Bool;
}

# What changed at a given version
enum ChangeKind {
  blockAdded @0;
  blockDeleted @1;
  edit @2;
  statusChange @3;
}

# Snapshot of a context version for timeline navigation
struct VersionSnapshot {
  version @0 :UInt64;         # Context version number
  timestamp @1 :UInt64;       # Unix millis when this version was created
  blockCount @2 :UInt32;      # Number of blocks at this version
  changeKind @3 :ChangeKind;
  changedBlockId @4 :BlockId; # The block that changed (if applicable)
}

# ============================================================================
# Context & Kernel Types
# ============================================================================

struct KernelInfo {
  id @0 :Data;                    # 16-byte KernelId (UUIDv7)
  name @1 :Text;
  userCount @2 :UInt32;
  agentCount @3 :UInt32;
  contexts @4 :List(ContextHandleInfo);
}

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
}

struct PresetInfo {
  id @0 :Data;                    # 16-byte PresetId (UUIDv7)
  label @1 :Text;
  description @2 :Text;
  provider @3 :Text;
  model @4 :Text;
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

# ============================================================================
# Drift Types (Multi-Context Communication)
# ============================================================================

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
# LLM Types
# ============================================================================

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
# Tool Types
# ============================================================================

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
# OutputData Types (Structured Output from kaish commands)
# ============================================================================

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
}

# ============================================================================
# Shell Types
# ============================================================================

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
  onOutput @0 (event :KernelOutputEvent);
}

# ============================================================================
# VFS Types
# ============================================================================

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

# ============================================================================
# Peer Types
# ============================================================================

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
  onRequest @0 (request :McpElicitationRequest) -> (response :McpElicitationResponse);
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
  onLog @0 (log :McpLogMessage);
}

# ============================================================================
# Kernel KV Events
# ============================================================================

# Callback interface for receiving kernel KV change events.
interface KvEvents {
  # A key changed. `deleted = true` means the key was removed (`value` empty);
  # otherwise `value` is the new value.
  onChange @0 (key :Text, value :Text, deleted :Bool);
}

# ============================================================================
# Server Interfaces
# ============================================================================

interface World {
  # Identity
  whoami @0 () -> (identity :Identity);

  # Kernel management
  listKernels @1 () -> (kernels :List(KernelInfo));
  bindKernel @2 (trace :TraceContext) -> (kernel :Kernel, kernelId :Data);
}

interface Kernel {
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

  # ==========================================================================
  # VFS & mounts
  # ==========================================================================
  vfs @14 () -> (vfs :Vfs);
  listMounts @15 () -> (mounts :List(MountInfo));
  mount @16 (path :Text, source :Text, writable :Bool);
  unmount @17 (path :Text) -> (success :Bool);

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

  # ==========================================================================
  # Blocks: queries & CRDT sync
  # ==========================================================================
  getContextState @34 (contextId :Data, trace :TraceContext) -> (state :ContextState);

  # Fetch blocks by query: all, byIds, or byFilter
  getBlocks @35 (contextId :Data, query :BlockQuery, trace :TraceContext) -> (blocks :List(BlockSnapshot));

  # Fetch CRDT sync state only (ops + version, no blocks)
  getContextSync @36 (contextId :Data, trace :TraceContext) -> (contextId :Data, ops :Data, version :UInt64);

  # Push CRDT operations from client to server for bidirectional sync.
  # Returns ack version so client knows ops were accepted and ordered.
  pushOps @37 (contextId :Data, ops :Data, trace :TraceContext) -> (ackVersion :UInt64);

  # Compact a context's oplog, bumping sync generation.
  # Connected clients will receive onSyncReset and must re-fetch full state.
  compactContext @38 (contextId :Data, trace :TraceContext) -> (newSize :UInt64, generation :UInt64);

  # ==========================================================================
  # Blocks: subscriptions
  # ==========================================================================
  subscribeBlocks @39 (callback :BlockEvents);

  # Subscribe to block events with server-side filtering.
  # Like subscribeBlocks but the server applies the filter before sending,
  # reducing bandwidth and client CPU during high-throughput streaming.
  subscribeBlocksFiltered @40 (callback :BlockEvents, filter :BlockEventFilter, instance :Text);

  # ==========================================================================
  # Blocks: mutation & curation
  # ==========================================================================
  # Set the excluded flag on a block (staging curation).
  setBlockExcluded @41 (contextId :Data, blockId :BlockId, excluded :Bool, trace :TraceContext) -> (ackVersion :UInt64);

  # Move a block to a new position. When `hasAfter` is true, `after` is
  # the block to land after; otherwise the block is parked at the
  # beginning of the document. Mirrors the `has_parent_id` idiom used
  # elsewhere in the schema.
  moveBlock @42 (contextId :Data, blockId :BlockId, hasAfter :Bool, after :BlockId, trace :TraceContext) -> (ackVersion :UInt64);

  # ==========================================================================
  # Turn control
  # ==========================================================================
  # Interrupt a running LLM stream or shell jobs for a context.
  # immediate=false → soft interrupt (stop agentic loop after current tool turn).
  # immediate=true  → hard interrupt (abort LLM stream + kill all kaish jobs).
  # Returns success=false when context has no active interrupt state (i.e. nothing running).
  interruptContext @43 (contextId :Data, immediate :Bool, trace :TraceContext) -> (success :Bool);

  # ==========================================================================
  # Input document (CRDT scratchpad per context)
  # ==========================================================================
  # Each context has a companion CRDT text document for compose input.
  # Any participant can read/write it. Submit snapshots to conversation block.

  # High-level edit: insert text at position, delete characters
  editInput @44 (contextId :Data, pos :UInt64, insert :Text, delete :UInt64, trace :TraceContext) -> (ackVersion :UInt64);

  # Full state fetch for join/reconnect recovery
  getInputState @45 (contextId :Data, trace :TraceContext) -> (content :Text, ops :Data, version :UInt64);

  # Raw DTE ops for CRDT-aware clients
  pushInputOps @46 (contextId :Data, ops :Data, trace :TraceContext) -> (ackVersion :UInt64);

  # Atomic submit: read input, create block, clear input.
  # Mode is explicit — no prefix detection.
  submitInput @47 (contextId :Data, mode :InputMode, trace :TraceContext) -> (commandBlockId :BlockId);

  # Clear the input document for a context (discard draft).
  # Emits InputCleared so all clients can reset their compose state.
  clearInput @48 (contextId :Data, trace :TraceContext) -> ();

  # ==========================================================================
  # Timeline navigation (fork-first temporal model)
  # ==========================================================================
  # The past is read-only, but forking is ubiquitous.

  # Cherry-pick a block into another context (carries lineage)
  cherryPickBlock @49 (sourceBlockId :BlockId, targetContextId :Data, trace :TraceContext) -> (newBlockId :BlockId);

  # Get context version history for timeline scrubber
  getContextHistory @50 (contextId :Data, limit :UInt32, trace :TraceContext) -> (snapshots :List(VersionSnapshot));

  # ==========================================================================
  # Semantic search & clustering
  # ==========================================================================
  # Semantic search: find contexts similar to a text query
  searchSimilar @51 (query :Text, k :UInt32, trace :TraceContext) -> (results :List(SimilarContext));

  # Context neighbors: find contexts similar to a given context
  getNeighbors @52 (contextId :Data, k :UInt32, trace :TraceContext) -> (results :List(SimilarContext));

  # Clustering: group contexts by semantic similarity
  getClusters @53 (minClusterSize :UInt32, trace :TraceContext) -> (clusters :List(ContextCluster));

  # ==========================================================================
  # Drift (cross-context communication with provenance)
  # ==========================================================================
  # List staged drifts pending flush
  driftQueue @54 () -> (staged :List(StagedDriftInfo));

  # Cancel a staged drift by ID
  driftCancel @55 (stagedId :UInt64) -> (success :Bool);

  # Inspect the drift router's dead-letter queue. Non-consuming — pairs with
  # replayDeadLetter. Drifts land here when MAX_DRIFT_RETRIES is exceeded or
  # the target context unregisters before delivery.
  listDeadLetters @56 (trace :TraceContext) -> (items :List(DeadLetter));

  # Replay a dead-letter item by id: extract from DLQ, reset retry count,
  # push back to the staging queue for another flush attempt. Returns
  # `replayed = false` when the id isn't present (already drained or
  # never queued).
  replayDeadLetter @57 (id :UInt64, trace :TraceContext) -> (replayed :Bool);

  # ==========================================================================
  # Configuration (config as CRDT)
  # ==========================================================================
  # Config files (theme.toml, bindings.toml, models.toml, etc.) are managed as CRDT documents.

  # List loaded config documents
  listConfigs @58 () -> (configs :List(Text));

  # Reload a config file from disk, discarding CRDT changes (safety valve)
  reloadConfig @59 (path :Text) -> (success :Bool, error :Text);

  # Reset a config file to embedded default
  resetConfig @60 (path :Text) -> (success :Bool, error :Text);

  # Get config content (from CRDT)
  getConfig @61 (path :Text) -> (content :Text, error :Text);

  # ==========================================================================
  # MCP (Model Context Protocol) management
  # ==========================================================================
  listMcpServers @62 () -> (servers :List(McpServerInfo));
  callMcpTool @63 (call :McpToolCall, trace :TraceContext) -> (result :McpToolResult);

  # MCP Resources (push-first with caching)
  listMcpResources @64 (server :Text, trace :TraceContext) -> (resources :List(McpResource));
  subscribeMcpResources @65 (callback :ResourceEvents, instance :Text);

  # MCP Roots (client advertises workspaces to servers)
  setMcpRoots @66 (roots :List(McpRoot));

  # MCP Prompts (poll-based with optional caching)
  listMcpPrompts @67 (server :Text) -> (prompts :List(McpPrompt));
  getMcpPrompt @68 (server :Text, name :Text, arguments :Text) -> (messages :List(McpPromptMessage));

  # MCP Progress (push-based streaming)
  subscribeMcpProgress @69 (callback :ProgressEvents);

  # MCP Elicitation (server-initiated requests for user input)
  subscribeMcpElicitations @70 (callback :ElicitationEvents, instance :Text);

  # MCP Completion (request/response)
  completeMcp @71 (server :Text, refType :Text, refName :Text, argName :Text, value :Text) -> (result :McpCompletionResult);

  # MCP Logging
  setMcpLogLevel @72 (server :Text, level :Text);
  subscribeMcpLogs @73 (callback :LoggingEvents);

  # MCP Cancellation
  cancelMcpRequest @74 (server :Text, requestId :Text);

  # ==========================================================================
  # Peer registry (drift navigation transport)
  # ==========================================================================
  # Peers are named RPC participants (the Bevy app, MCP servers, future
  # clients) that the kernel can dispatch invocations to. The transport is
  # how MCP-driven agents tell the app to switch contexts, etc.

  # Attach a peer to this kernel.
  # `commands` is the callback the kernel uses to invoke this peer.
  attachPeer @75 (config :PeerConfig, commands :PeerCommands) -> (info :PeerInfo);

  # List all attached peers on this kernel.
  listPeers @76 () -> (peers :List(PeerInfo));

  # Detach a peer from this kernel.
  detachPeer @77 (nick :Text);

  # Invoke a peer. Params and result are JSON bytes.
  invokePeer @78 (nick :Text, action :Text, params :Data) -> (result :Data);

  # ==========================================================================
  # Kernel key–value store (persistent, synced env — docs/kernel-kv.md)
  # ==========================================================================
  # A small, durable, collaborative key→string store. Keys are flat UTF-8
  # strings (dotted namespaces are convention, not mechanism); values are
  # strings (structured data is the caller's JSON). No per-key ACLs — a
  # single-user shared-trust kernel.

  # Read a key. `found = false` when absent, deleted, or advisory-expired.
  kvGet @79 (key :Text) -> (value :Text, found :Bool);

  # Set a key. `hasExpiresAt` gates the advisory absolute expiry (writer-clock
  # ms). Returns an error string on the 64 KB value cap or a persistence fault.
  kvSet @80 (key :Text, value :Text, hasExpiresAt :Bool, expiresAt :Int64) -> (success :Bool, error :Text);

  # Delete a key. `existed` reports whether a live value was present.
  kvDelete @81 (key :Text) -> (existed :Bool);

  # List keys, optionally filtered by prefix. `nextCursor` is reserved for
  # future pagination (always absent in v1).
  kvKeys @82 (prefix :Text, hasPrefix :Bool) -> (keys :List(Text), nextCursor :Text, hasNextCursor :Bool);

  # Subscribe to whole-store changes (callback fires per set/delete). The
  # client filters by prefix; v1 streams the whole store.
  kvWatch @83 (callback :KvEvents);

  # ==========================================================================
  # In-app editor sessions (the vi/edit builtin; see docs/vi.md)
  # ==========================================================================
  # A kernel-owned vi session bound to the CRDT block that owns a path's text.
  # Session ids are global (no contextId needed); the path resolves the owner.
  # Renderers draw `editorState`/`subscribeEditor` and forward keys to
  # `editorKeys`. Edits mirror onto the CRDT block — rc/config permission errors
  # surface here loudly (crash over corruption).
  editorOpen @84 (path :Text, trace :TraceContext) -> (state :EditorState);
  editorKeys @85 (sessionId :UInt64, keys :Text, trace :TraceContext) -> (state :EditorState);
  editorState @86 (sessionId :UInt64, trace :TraceContext) -> (state :EditorState);
  editorSave @87 (sessionId :UInt64, trace :TraceContext) -> (state :EditorState);
  editorQuit @88 (sessionId :UInt64, trace :TraceContext) -> ();
  # Push channel: server streams editor state changes (incl. future remote merges).
  subscribeEditor @89 (callback :EditorEvents);

  # ==========================================================================
  # Per-client durable view state (docs/shared-state.md "Retiring KV")
  # ==========================================================================
  # The kernel-managed replacement for the app's one production KV use:
  # "reopen the context this window was last looking at" on reconnect. Backed
  # by a small normalized `client_views` KernelDb row keyed by the app's
  # stable per-installation client id — not a stringly KV namespace.

  # Record the last-viewed context for `clientId`. Idempotent re-write on the
  # same value is harmless (the app fires this on every context switch,
  # including the restore itself).
  setLastContext @90 (clientId :Text, contextId :Text);

  # Read back the last-viewed context for `clientId`. `found = false` when no
  # view is on record for this client (matches `kvGet`/`getShellVar`'s
  # value+found shape rather than an empty-string sentinel).
  getClientView @91 (clientId :Text) -> (contextId :Text, found :Bool);
}

# ============================================================================
# VFS Interface
# ============================================================================

interface Vfs {
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
}
