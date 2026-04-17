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
  agentId @1 :Data;     # 16-byte PrincipalId (UUIDv7)
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

# Block content type — 9 variants covering what a block *is*.
# Mechanism metadata lives in companion enums:
# - ToolKind on ToolCall/ToolResult: which execution engine (Shell, Mcp, Builtin)
# - DriftKind on Drift: how content transferred (Push, Pull, Merge, Distill, Commit)
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
}

# Which execution engine handled a tool call/result.
enum ToolKind {
  shell @0;     # kaish shell execution (shell_execute RPC)
  mcp @1;       # MCP tool invocation (via registered MCP server)
  builtin @2;   # Kernel builtin tool (no external process)
}

# Submit routing: explicit mode instead of prefix-sniffing.
enum InputMode {
  chat @0;
  shell @1;
}

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

# How a drift block arrived from another context.
enum DriftKind {
  push @0;          # User manually pushed content
  pull @1;          # User pulled/requested content
  merge @2;         # Context merge (fork coming home)
  distill @3;       # LLM-summarized before transfer
  commit @4;        # Git commit recorded as conversation provenance
  notification @5;  # External notification (MCP resource updates, system events)
  fork @6;          # Fork marker (ephemeral, summarizes the fork operation)
}

# Flat block snapshot — all fields present, some unused depending on kind.
struct BlockSnapshot {
  # Core identity
  id @0 :BlockId;

  # DAG structure
  parentId @1 :BlockId;
  hasParentId @2 :Bool;       # True if parentId is set

  # Metadata (author derived from id.agentId — no separate field)
  role @3 :Role;
  status @4 :Status;
  kind @5 :BlockKind;
  createdAt @6 :UInt64;       # Unix timestamp in milliseconds

  # Content (all blocks)
  content @7 :Text;           # Main text content
  collapsed @8 :Bool;         # For thinking blocks

  # Tool-specific fields (toolCall)
  toolName @9 :Text;          # Tool name for toolCall
  toolInput @10 :Text;        # JSON-encoded input for toolCall

  # Tool-specific fields (toolResult)
  toolCallId @11 :BlockId;    # Reference to parent toolCall block
  exitCode @12 :Int32;        # Exit code for toolResult
  hasExitCode @13 :Bool;      # True if exitCode is set (Int32 value type)
  isError @14 :Bool;          # True if toolResult is an error

  # Structured output data for richer formatting (typed Cap'n Proto struct)
  outputData @15 :OutputData;

  # Drift-specific fields (cross-context transfer)
  sourceContext @16 :Data;    # 16-byte ContextId of originating context
  sourceModel @17 :Text;      # Model that produced this content
  driftKind @18 :DriftKind;   # How this block arrived (push/pull/merge/distill/commit)
  hasDriftKind @19 :Bool;     # True if driftKind is set (enum default 0=push is valid)

  # Tool mechanism metadata (ToolCall / ToolResult)
  toolKind @20 :ToolKind;     # Which execution engine (shell/mcp/builtin)
  hasToolKind @21 :Bool;      # True if toolKind is set (enum default 0=shell is valid)

  # File metadata (File blocks)
  filePath @22 :Text;         # Path for file-kind blocks (e.g. "/src/main.rs")

  # LLM-assigned tool invocation ID (e.g. "toolu_01ABC...")
  toolUseId @23 :Text;        # Present on ToolCall and ToolResult blocks

  # MIME content type hint (e.g. "text/markdown", "image/svg+xml")
  contentType @24 :Text;      # Optional — consumers fall back to heuristic detection

  # Ephemeral blocks are displayed but excluded from LLM hydration.
  # Use for human-only output (help text, status info) that wastes model context.
  ephemeral @25 :Bool;

  # User-curated exclusion — block is omitted from hydration.
  # Unlike ephemeral (system-managed), toggled by the user during staging.
  excluded @26 :Bool;

  # Error-specific fields (Error blocks)
  errorPayload @27 :ErrorPayload;
  hasErrorPayload @28 :Bool;

  # Notification-specific fields (Notification blocks)
  notificationPayload @29 :NotificationPayload;
  hasNotificationPayload @30 :Bool;

  # Resource-specific fields (Resource blocks)
  resourcePayload @31 :ResourcePayload;
  hasResourcePayload @32 :Bool;
}

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
  tool @4 :Text;                # ToolAdded/Removed — empty = None
  hasCount @5 :Bool;            # True iff `count` applies (Coalesced)
  count @6 :UInt32;
  detail @7 :Text;              # Expandable body — empty = None
}

# Structured resource payload for Resource blocks (Phase 3 — D-42).
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

# Full context state — blocks + CRDT oplog for sync
struct ContextState {
  contextId @0 :Data;   # 16-byte ContextId (UUIDv7)
  blocks @1 :List(BlockSnapshot);
  version @2 :UInt64;
  ops @3 :Data;         # Full oplog bytes for CRDT sync
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

# Callback for receiving block updates from server
interface BlockEvents {
  onBlockInserted @0 (contextId :Data, block :BlockSnapshot, afterId :BlockId, hasAfterId :Bool, ops :Data);
  onBlockDeleted @1 (contextId :Data, blockId :BlockId);
  onBlockCollapsed @2 (contextId :Data, blockId :BlockId, collapsed :Bool);
  onBlockMoved @3 (contextId :Data, blockId :BlockId, afterId :BlockId, hasAfterId :Bool);
  onBlockStatusChanged @4 (contextId :Data, blockId :BlockId, status :Status, outputData :OutputData);
  onBlockTextOps @5 (contextId :Data, blockId :BlockId, ops :Data);
  onSyncReset @6 (contextId :Data, generation :UInt64);

  # Input document events (compose scratchpad)
  onInputTextOps @7 (contextId :Data, ops :Data);
  onInputCleared @8 (contextId :Data);

  # Session control events (server → client)
  onContextSwitched @9 (contextId :Data);

  # Block excluded flag changed (staging curation)
  onBlockExcludedChanged @10 (contextId :Data, blockId :BlockId, excluded :Bool);
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
  contextState @11 :Text;         # "live"/"staging"/"archived" (default "live")
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
}

struct KernelConfig {
  name @0 :Text;
  mounts @1 :List(MountSpec);
  consentMode @2 :ConsentMode;
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
  stdout @2 :Data;          # Raw bytes (may be binary — images, postcard, etc.)
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
    blob @6 :Text;       # BlobRef path
  }
}

# Shell variable name + value pair
struct ShellVar {
  name @0 :Text;
  value @1 :ShellValue;
}

# Reference to binary data in blob storage (/v/blobs/{id})
struct BlobRef {
  id @0 :Text;              # Unique blob identifier
  size @1 :UInt64;          # Size in bytes
  contentType @2 :Text;     # MIME type (e.g., "image/png")
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
# Agent Types
# ============================================================================

# Configuration for attaching an agent
struct AgentConfig {
  nick @0 :Text;              # Display name: "spell-check", "claude-review"
  instance @1 :Text;          # Instance ID: "quick", "deep", "haiku"
  provider @2 :Text;          # LLM provider: "anthropic", "openai", "local"
  modelId @3 :Text;           # Model: "claude-3-haiku", "gpt-4-mini"
  capabilities @4 :List(AgentCapability);  # What this agent can do
}

# Information about an attached agent
struct AgentInfo {
  nick @0 :Text;              # Display name
  instance @1 :Text;          # Instance ID
  provider @2 :Text;          # LLM provider
  modelId @3 :Text;           # Model
  capabilities @4 :List(AgentCapability);  # What this agent can do
  status @5 :AgentStatus;     # Current agent status
  attachedAt @6 :UInt64;      # Unix timestamp ms
  lastActivity @7 :UInt64;    # Unix timestamp ms
}

# Agent capabilities — what actions an agent can perform
enum AgentCapability {
  spellCheck @0;              # Quick spell-checking
  grammar @1;                 # Grammar correction
  format @2;                  # Code/text formatting
  review @3;                  # Code/content review (slower, thoughtful)
  generate @4;                # Content generation
  refactor @5;                # Code refactoring suggestions
  explain @6;                 # Explain selected content
  translate @7;               # Translation to other languages
  summarize @8;               # Summarize long content
  custom @9;                  # Custom capability (specified via action string)
}

# Agent status
enum AgentStatus {
  ready @0;                   # Available to handle requests
  busy @1;                    # Currently processing a request
  offline @2;                 # Not currently responding
}

# Agent activity event
struct AgentActivityEvent {
  agent @0 :Text;             # Agent nick
  union {
    started :group {          # Agent started working on content
      blockId @1 :BlockId;
      action @2 :Text;
    }
    progress :group {         # Agent is making progress
      blockId @3 :BlockId;
      message @4 :Text;
      percent @5 :Float32;    # 0.0 to 1.0
    }
    completed :group {        # Agent finished
      blockId @6 :BlockId;
      success @7 :Bool;
    }
    cursorMoved :group {      # Agent cursor position changed
      blockId @8 :BlockId;
      offset @9 :UInt64;
    }
  }
}

# Callback for receiving agent activity events
interface AgentEvents {
  onActivity @0 (event :AgentActivityEvent);
}

# Callback for receiving agent invocations (reverse RPC).
# Registered via attachAgent; the kernel calls back to dispatch work.
# Same pattern as MCP sampling: server holds callback to client.
interface AgentCommands {
  invoke @0 (action :Text, params :Data) -> (result :Data);
}

# ============================================================================
# MCP (Model Context Protocol) Types
# ============================================================================

struct McpServerConfig {
  name @0 :Text;              # Unique name for this server (e.g., "git", "exa")
  command @1 :Text;           # Command to run (e.g., "uvx", "npx") — stdio only
  args @2 :List(Text);        # Arguments for the command — stdio only
  env @3 :List(EnvVar);       # Environment variables
  cwd @4 :Text;               # Working directory (optional) — stdio only
  transport @5 :Text;         # "stdio" (default) or "streamable_http"
  url @6 :Text;               # Server URL — streamable_http only
}

struct EnvVar {
  key @0 :Text;
  value @1 :Text;
}

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
  server @0 :Text;            # Server name (e.g., "git")
  tool @1 :Text;              # Tool name (e.g., "git_status")
  arguments @2 :Text;         # JSON-encoded arguments
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
# Server Interfaces
# ============================================================================

interface World {
  # Identity
  whoami @0 () -> (identity :Identity);

  # Kernel management
  listKernels @1 () -> (kernels :List(KernelInfo));
  attachKernel @2 (trace :TraceContext) -> (kernel :Kernel, kernelId :Data);
}

interface Kernel {
  # Info
  getInfo @0 (trace :TraceContext) -> (info :KernelInfo);

  # kaish execution
  execute @1 (code :Text, trace :TraceContext) -> (execId :UInt64);
  interrupt @2 (execId :UInt64, trace :TraceContext);
  complete @3 (partial :Text, cursor :UInt32, trace :TraceContext) -> (completions :List(Completion));
  subscribeOutput @4 (callback :KernelOutput);
  getCommandHistory @5 (limit :UInt32, trace :TraceContext) -> (entries :List(HistoryEntry));

  # VFS
  vfs @6 () -> (vfs :Vfs);
  listMounts @7 () -> (mounts :List(MountInfo));
  mount @8 (path :Text, source :Text, writable :Bool);
  unmount @9 (path :Text) -> (success :Bool);

  # Tool execution
  executeTool @10 (call :ToolCall, trace :TraceContext) -> (result :ToolResult);
  getToolSchemas @11 (trace :TraceContext) -> (schemas :List(ToolSchema));

  # Block-based CRDT operations
  # Slot @12 was `applyBlockOp` — removed when blocks moved to CRDT sync via
  # pushOps. Cap'n Proto disallows ordinal holes, so @12 remains as a no-op.
  removedApplyBlockOp @12 () -> ();
  subscribeBlocks @13 (callback :BlockEvents);
  getContextState @14 (contextId :Data, trace :TraceContext) -> (state :ContextState);

  # LLM operations
  prompt @15 (request :LlmRequest, trace :TraceContext) -> (promptId :Text);

  # Context management (ContextId = 16-byte UUIDv7 as Data)
  listContexts @16 (trace :TraceContext) -> (contexts :List(ContextHandleInfo));
  createContext @17 (label :Text) -> (id :Data);
  joinContext @18 (contextId :Data, instance :Text, trace :TraceContext) -> (contextId :Data);

  # MCP (Model Context Protocol) management
  registerMcp @19 (config :McpServerConfig) -> (info :McpServerInfo);
  unregisterMcp @20 (name :Text);
  listMcpServers @21 () -> (servers :List(McpServerInfo));
  callMcpTool @22 (call :McpToolCall, trace :TraceContext) -> (result :McpToolResult);

  # Shell execution (kaish REPL with block output)
  # Creates ToolCall (ToolKind::Shell) and ToolResult (ToolKind::Shell) blocks, streams output via BlockEvents
  shellExecute @23 (code :Text, contextId :Data, trace :TraceContext, userInitiated :Bool) -> (commandBlockId :BlockId);

  # Shell state (kaish working directory and last result)
  getCwd @24 () -> (path :Text);
  setCwd @25 (path :Text) -> (success :Bool, error :Text);
  getLastResult @26 () -> (result :ShellExecResult);

  # Blob storage (in-memory, size-capped)
  # Blobs are stored at /v/blobs/{id} in kaish's VFS
  writeBlob @27 (data :Data, contentType :Text) -> (ref :BlobRef);
  readBlob @28 (id :Text) -> (data :Data);
  deleteBlob @29 (id :Text) -> (success :Bool);
  listBlobs @30 () -> (refs :List(BlobRef));

  # Push CRDT operations from client to server for bidirectional sync
  # Returns ack version so client knows ops were accepted and ordered
  pushOps @31 (contextId :Data, ops :Data, trace :TraceContext) -> (ackVersion :UInt64);

  # MCP Resource management (push-first with caching)
  listMcpResources @32 (server :Text, trace :TraceContext) -> (resources :List(McpResource));
  readMcpResource @33 (server :Text, uri :Text, trace :TraceContext) -> (contents :McpResourceContents, hasContents :Bool);
  subscribeMcpResources @34 (callback :ResourceEvents);

  # MCP Roots (client advertises workspaces to servers)
  setMcpRoots @35 (roots :List(McpRoot));

  # MCP Prompts (poll-based with optional caching)
  listMcpPrompts @36 (server :Text) -> (prompts :List(McpPrompt));
  getMcpPrompt @37 (server :Text, name :Text, arguments :Text) -> (messages :List(McpPromptMessage));

  # MCP Progress (push-based streaming)
  subscribeMcpProgress @38 (callback :ProgressEvents);

  # MCP Elicitation (server-initiated requests for user input)
  subscribeMcpElicitations @39 (callback :ElicitationEvents);

  # MCP Completion (request/response)
  completeMcp @40 (server :Text, refType :Text, refName :Text, argName :Text, value :Text) -> (result :McpCompletionResult);

  # MCP Logging
  setMcpLogLevel @41 (server :Text, level :Text);
  subscribeMcpLogs @42 (callback :LoggingEvents);

  # MCP Cancellation
  cancelMcpRequest @43 (server :Text, requestId :Text);

  # ============================================================================
  # Agent Attachment
  # ============================================================================
  # Agents are autonomous participants that can edit content alongside humans.

  # Attach an agent to this kernel.
  # The optional `commands` callback enables kernel → agent invocation.
  attachAgent @44 (config :AgentConfig, commands :AgentCommands) -> (info :AgentInfo);

  # List all attached agents on this kernel
  listAgents @45 () -> (agents :List(AgentInfo));

  # Detach an agent from this kernel
  detachAgent @46 (nick :Text);

  # Update agent capabilities
  setAgentCapabilities @47 (nick :Text, capabilities :List(AgentCapability));

  # Invoke an agent's capability. Params and result are JSON bytes.
  invokeAgent @48 (nick :Text, action :Text, params :Data) -> (result :Data);

  # Subscribe to agent activity events
  subscribeAgentEvents @49 (callback :AgentEvents);

  # ============================================================================
  # Timeline Navigation (Fork-First Temporal Model)
  # ============================================================================
  # Navigate through context history with fork and cherry-pick operations.
  # The past is read-only, but forking is ubiquitous.

  # Removed: use `kj fork` instead. Ordinal kept for schema compatibility.
  forkFromVersionRemoved @50 () -> ();

  # Cherry-pick a block into another context (carries lineage)
  cherryPickBlock @51 (sourceBlockId :BlockId, targetContextId :Data, trace :TraceContext) -> (newBlockId :BlockId);

  # Get context version history for timeline scrubber
  getContextHistory @52 (contextId :Data, limit :UInt32, trace :TraceContext) -> (snapshots :List(VersionSnapshot));

  # ============================================================================
  # Configuration (Config as CRDT)
  # ============================================================================
  # Config files (theme.toml, bindings.toml, models.toml, etc.) are managed as CRDT documents.

  # List loaded config documents
  listConfigs @53 () -> (configs :List(Text));

  # Reload a config file from disk, discarding CRDT changes (safety valve)
  reloadConfig @54 (path :Text) -> (success :Bool, error :Text);

  # Reset a config file to embedded default
  resetConfig @55 (path :Text) -> (success :Bool, error :Text);

  # Get config content (from CRDT)
  getConfig @56 (path :Text) -> (content :Text, error :Text);

  # ============================================================================
  # Multi-Context Drifting (Cross-Context Communication)
  # ============================================================================
  # Drift enables content transfer between kernel contexts with provenance tracking.

  # Get this kernel's context ID and label
  getContextId @57 (trace :TraceContext) -> (id :Data, label :Text);

  # Configure LLM provider/model for a specific context (per-context model assignment).
  # If contextId is empty/missing, uses the connection's current context.
  configureLlm @58 (provider :Text, model :Text, trace :TraceContext, contextId :Data) -> (success :Bool, error :Text);

  # REMOVED pre-1.0: use shell_execute("kj drift push ...") instead
  driftPush @59 (targetCtx :Data, content :Text, summarize :Bool, trace :TraceContext) -> (stagedId :UInt64);

  # REMOVED pre-1.0: use shell_execute("kj drift flush") instead
  driftFlush @60 (trace :TraceContext) -> (count :UInt32);

  # List staged drifts pending flush
  driftQueue @61 () -> (staged :List(StagedDriftInfo));

  # Cancel a staged drift by ID
  driftCancel @62 (stagedId :UInt64) -> (success :Bool);

  # REMOVED pre-1.0: use shell_execute("kj drift pull ...") instead
  driftPull @63 (sourceCtx :Data, prompt :Text, trace :TraceContext) -> (blockId :BlockId);

  # REMOVED pre-1.0: use shell_execute("kj drift merge ...") instead
  driftMerge @64 (sourceCtx :Data, trace :TraceContext) -> (blockId :BlockId);

  # Rename a context's label
  renameContext @65 (contextId :Data, label :Text);

  # ============================================================================
  # LLM Configuration (Per-Kernel Multi-Provider LLM)
  # ============================================================================

  # Get current LLM configuration for this kernel
  getLlmConfig @66 (trace :TraceContext) -> (config :LlmConfigInfo);

  # Set default LLM provider
  setDefaultProvider @67 (provider :Text) -> (success :Bool, error :Text);

  # Set default model for a provider
  setDefaultModel @68 (provider :Text, model :Text) -> (success :Bool, error :Text);

  # ============================================================================
  # Tool Filter Configuration
  # ============================================================================

  # Phase 5 D-54: tool filter retired. `ContextToolBinding` +
  # `HookPhase::ListTools` subsume allow/deny semantics. Ordinals kept for
  # schema compatibility; methods are no-ops.
  getToolFilterRemoved @69 () -> ();
  setToolFilterRemoved @70 () -> ();

  # ============================================================================
  # Shell Variable Introspection (kaish scope)
  # ============================================================================

  # Get a shell variable by name
  getShellVar @71 (name :Text) -> (value :ShellValue, found :Bool);

  # Set a shell variable
  setShellVar @72 (name :Text, value :ShellValue) -> (success :Bool, error :Text);

  # List all shell variables
  listShellVars @73 () -> (vars :List(ShellVar));

  # Compact a context's oplog, bumping sync generation.
  # Connected clients will receive onSyncReset and must re-fetch full state.
  compactContext @74 (contextId :Data, trace :TraceContext) -> (newSize :UInt64, generation :UInt64);

  # ============================================================================
  # Input Document (CRDT scratchpad per context)
  # ============================================================================
  # Each context has a companion CRDT text document for compose input.
  # Any participant can read/write it. Submit snapshots to conversation block.

  # High-level edit: insert text at position, delete characters
  editInput @75 (contextId :Data, pos :UInt64, insert :Text, delete :UInt64, trace :TraceContext) -> (ackVersion :UInt64);

  # Full state fetch for join/reconnect recovery
  getInputState @76 (contextId :Data, trace :TraceContext) -> (content :Text, ops :Data, version :UInt64);

  # Raw DTE ops for CRDT-aware clients
  pushInputOps @77 (contextId :Data, ops :Data, trace :TraceContext) -> (ackVersion :UInt64);

  # Atomic submit: read input, create block, clear input.
  # Mode is explicit — no prefix detection.
  submitInput @78 (contextId :Data, mode :InputMode, trace :TraceContext) -> (commandBlockId :BlockId);

  # Semantic search: find contexts similar to a text query
  searchSimilar @79 (query :Text, k :UInt32, trace :TraceContext) -> (results :List(SimilarContext));

  # Context neighbors: find contexts similar to a given context
  getNeighbors @80 (contextId :Data, k :UInt32, trace :TraceContext) -> (results :List(SimilarContext));

  # Clustering: group contexts by semantic similarity
  getClusters @81 (minClusterSize :UInt32, trace :TraceContext) -> (clusters :List(ContextCluster));

  # ============================================================================
  # Block Queries (replaces getContextState for block-only fetches)
  # ============================================================================

  # Fetch blocks by query: all, byIds, or byFilter
  getBlocks @82 (contextId :Data, query :BlockQuery, trace :TraceContext) -> (blocks :List(BlockSnapshot));

  # Fetch CRDT sync state only (ops + version, no blocks)
  getContextSync @83 (contextId :Data, trace :TraceContext) -> (contextId :Data, ops :Data, version :UInt64);

  # ============================================================================
  # Filtered Block Subscriptions
  # ============================================================================

  # Subscribe to block events with server-side filtering.
  # Like subscribeBlocks @13 but the server applies the filter before sending,
  # reducing bandwidth and client CPU during high-throughput streaming.
  subscribeBlocksFiltered @84 (callback :BlockEvents, filter :BlockEventFilter);

  # ============================================================================
  # Per-Context Tool Filter
  # ============================================================================

  # Phase 5 D-54: retired. See `builtin.bindings` MCP tools for the
  # replacement. Ordinals kept for schema compatibility.
  setContextToolFilterRemoved @85 () -> ();
  getContextToolFilterRemoved @86 () -> ();

  # Removed: use `kj fork` instead. Ordinal kept for schema compatibility.
  forkFilteredRemoved @87 () -> ();

  # Interrupt a running LLM stream or shell jobs for a context.
  # immediate=false → soft interrupt (stop agentic loop after current tool turn).
  # immediate=true  → hard interrupt (abort LLM stream + kill all kaish jobs).
  # Returns success=false when context has no active interrupt state (i.e. nothing running).
  interruptContext @88 (contextId :Data, immediate :Bool, trace :TraceContext) -> (success :Bool);

  # List all presets for this kernel
  listPresets @89 (trace :TraceContext) -> (presets :List(PresetInfo));

  # Clear the input document for a context (discard draft).
  # Emits InputCleared so all clients can reset their compose state.
  clearInput @90 (contextId :Data, trace :TraceContext) -> ();

  # Set lifecycle state for a context (e.g., Staging → Live).
  setContextState @91 (contextId :Data, state :Text, trace :TraceContext) -> (success :Bool, error :Text);

  # Set the excluded flag on a block (staging curation).
  setBlockExcluded @92 (contextId :Data, blockId :BlockId, excluded :Bool, trace :TraceContext) -> (ackVersion :UInt64);
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
