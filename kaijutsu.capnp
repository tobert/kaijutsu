@0xb8e3f4a9c2d1e0f7;

# Kaijutsu RPC Schema
# Cap'n Proto interface for kernel-based collaborative spaces

# ============================================================================
# Distributed Tracing (W3C Trace Context)
# ============================================================================

# W3C Trace Context for distributed tracing across SSH boundary.
# See: https://www.w3.org/TR/trace-context/
# Cap'n Proto schema evolution: old clients without this field still work —
# the server sees default (empty) strings and creates a root span.
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
}

# Execution/processing status
enum Status {
  pending @0;
  running @1;
  done @2;
  error @3;
}

# Block content type — 5 variants covering what a block *is*.
# Mechanism metadata lives in companion enums:
# - ToolKind on ToolCall/ToolResult: which execution engine (Shell, Mcp, Builtin)
# - DriftKind on Drift: how content transferred (Push, Pull, Merge, Distill, Commit)
enum BlockKind {
  text @0;
  thinking @1;
  toolCall @2;
  toolResult @3;
  drift @4;
}

# Which execution engine handled a tool call/result.
enum ToolKind {
  shell @0;     # kaish shell execution (shell_execute RPC)
  mcp @1;       # MCP tool invocation (via registered MCP server)
  builtin @2;   # Kernel builtin tool (no external process)
}

# How a drift block arrived from another context.
enum DriftKind {
  push @0;      # User manually pushed content
  pull @1;      # User pulled/requested content
  merge @2;     # Context merge (fork coming home)
  distill @3;   # LLM-summarized before transfer
  commit @4;    # Git commit recorded as conversation provenance
}

# Flat block snapshot — all fields present, some unused depending on kind.
struct BlockSnapshot {
  # Core identity
  id @0 :BlockId;

  # DAG structure
  parentId @1 :BlockId;
  hasParentId @2 :Bool;       # True if parentId is set

  # Metadata
  role @3 :Role;
  status @4 :Status;
  kind @5 :BlockKind;
  author @6 :Data;            # 16-byte PrincipalId (UUIDv7)
  createdAt @7 :UInt64;       # Unix timestamp in milliseconds

  # Content (all blocks)
  content @8 :Text;           # Main text content
  collapsed @9 :Bool;         # For thinking blocks

  # Tool-specific fields (toolCall)
  toolName @10 :Text;         # Tool name for toolCall
  toolInput @11 :Text;        # JSON-encoded input for toolCall

  # Tool-specific fields (toolResult)
  toolCallId @12 :BlockId;    # Reference to parent toolCall block
  hasToolCallId @13 :Bool;    # True if toolCallId is set
  exitCode @14 :Int32;        # Exit code for toolResult
  hasExitCode @15 :Bool;      # True if exitCode is set
  isError @16 :Bool;          # True if toolResult is an error

  # Display hint for richer output formatting (JSON-serialized)
  displayHint @17 :Text;      # JSON DisplayHint for tables/trees
  hasDisplayHint @18 :Bool;   # True if displayHint is set

  # Drift-specific fields (cross-context transfer)
  sourceContext @19 :Data;    # 16-byte ContextId of originating context
  hasSourceContext @20 :Bool; # True if sourceContext is set
  sourceModel @21 :Text;      # Model that produced this content
  hasSourceModel @22 :Bool;   # True if sourceModel is set
  driftKind @23 :DriftKind;   # How this block arrived (push/pull/merge/distill/commit)
  hasDriftKind @24 :Bool;     # True if driftKind is set

  # Tool mechanism metadata (ToolCall / ToolResult)
  toolKind @25 :ToolKind;     # Which execution engine (shell/mcp/builtin)
  hasToolKind @26 :Bool;      # True if toolKind is set
}

# Operations on block documents
struct BlockDocOp {
  union {
    # Insert a new block
    insertBlock :group {
      block @0 :BlockSnapshot;
      afterId @1 :BlockId;
      hasAfterId @2 :Bool;    # False = insert at start
    }
    # Delete a block
    deleteBlock @3 :BlockId;
    # Edit text within a block (Thinking/Text only)
    editBlockText :group {
      id @4 :BlockId;
      pos @5 :UInt64;
      insert @6 :Text;
      delete @7 :UInt64;
    }
    # Toggle collapsed state (Thinking only)
    setCollapsed :group {
      id @8 :BlockId;
      collapsed @9 :Bool;
    }
    # Update block status
    setStatus :group {
      id @10 :BlockId;
      status @11 :Status;
    }
    # Move block to new position
    moveBlock :group {
      id @12 :BlockId;
      afterId @13 :BlockId;
      hasAfterId @14 :Bool;
    }
  }
}

# Full document state with blocks
struct DocumentState {
  documentId @0 :Text;
  blocks @1 :List(BlockSnapshot);
  version @2 :UInt64;
  ops @3 :Data;  # Full oplog bytes for CRDT sync
}

# Snapshot of a document version for timeline navigation
struct VersionSnapshot {
  version @0 :UInt64;         # Document version number
  timestamp @1 :UInt64;       # Unix millis when this version was created
  blockCount @2 :UInt32;      # Number of blocks at this version
  changeKind @3 :Text;        # Type of change: "block_added", "edit", "status_change"
  changedBlockId @4 :BlockId; # The block that changed (if applicable)
}

# Callback for receiving block updates from server
interface BlockEvents {
  onBlockInserted @0 (documentId :Text, block :BlockSnapshot, afterId :BlockId, hasAfterId :Bool, ops :Data);
  onBlockDeleted @1 (documentId :Text, blockId :BlockId);
  onBlockCollapsed @2 (documentId :Text, blockId :BlockId, collapsed :Bool);
  onBlockMoved @3 (documentId :Text, blockId :BlockId, afterId :BlockId, hasAfterId :Bool);
  onBlockStatusChanged @4 (documentId :Text, blockId :BlockId, status :Status);
  onBlockTextOps @5 (documentId :Text, blockId :BlockId, ops :Data);
  onSyncReset @6 (documentId :Text, generation :UInt64);
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

# A document attached to a context
struct ContextDocument {
  id @0 :Text;                      # Block/document ID
  attachedBy @1 :Text;              # Who attached it
  attachedAt @2 :UInt64;            # When it was attached (unix timestamp ms)
}

# A context within a kernel
struct Context {
  id @0 :Data;                    # 16-byte ContextId (UUIDv7)
  label @1 :Text;                 # Optional human-friendly name
  documents @2 :List(ContextDocument);
}

# Information about a registered context (used by listContexts, KernelInfo)
struct ContextHandleInfo {
  id @0 :Data;                    # 16-byte ContextId (UUIDv7) — the real identity
  label @1 :Text;                 # Optional human-friendly name (mutable)
  parentId @2 :Data;              # 16-byte ContextId of parent (empty = no parent)
  provider @3 :Text;
  model @4 :Text;
  createdAt @5 :UInt64;
  documentId @6 :Text;            # Primary document ID for this context
  traceId @7 :Data;               # 16-byte OTel trace ID for context-scoped tracing
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
  documentId @2 :Text;    # Target document for response blocks
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
  stdout @2 :Data;          # Raw stdout bytes
  stderr @3 :Text;          # Stderr as text
  data @4 :Text;            # JSON-encoded parsed data (if applicable)
  hint @5 :Text;            # JSON-encoded DisplayHint for tables/trees
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
    structured @3 :Text;        # JSON structured output
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
  success @0 :Bool;
  content @1 :Text;           # Result content (text)
  isError @2 :Bool;           # True if the tool returned an error
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
  # @3 reserved (was createKernel — kernel forking downplayed)
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

  # Lifecycle
  fork @6 (name :Text) -> (kernel :Kernel);
  thread @7 (name :Text) -> (kernel :Kernel);
  detach @8 ();

  # VFS
  vfs @9 () -> (vfs :Vfs);
  listMounts @10 () -> (mounts :List(MountInfo));
  mount @11 (path :Text, source :Text, writable :Bool);
  unmount @12 (path :Text) -> (success :Bool);

  # Tool execution
  executeTool @13 (call :ToolCall, trace :TraceContext) -> (result :ToolResult);
  getToolSchemas @14 (trace :TraceContext) -> (schemas :List(ToolSchema));

  # Block-based CRDT operations
  applyBlockOp @15 (documentId :Text, op :BlockDocOp) -> (newVersion :UInt64);
  subscribeBlocks @16 (callback :BlockEvents);
  getDocumentState @17 (documentId :Text, trace :TraceContext) -> (state :DocumentState);

  # LLM operations
  prompt @18 (request :LlmRequest, trace :TraceContext) -> (promptId :Text);

  # Context management (ContextId = 16-byte UUIDv7 as Data)
  listContexts @19 (trace :TraceContext) -> (contexts :List(ContextHandleInfo));
  createContext @20 (label :Text) -> (id :Data);
  joinContext @21 (contextId :Data, instance :Text, trace :TraceContext) -> (documentId :Text);
  attachDocument @22 (contextId :Data, documentId :Text);
  detachDocument @23 (contextId :Data, documentId :Text);

  # MCP (Model Context Protocol) management
  registerMcp @24 (config :McpServerConfig) -> (info :McpServerInfo);
  unregisterMcp @25 (name :Text);
  listMcpServers @26 () -> (servers :List(McpServerInfo));
  callMcpTool @27 (call :McpToolCall, trace :TraceContext) -> (result :McpToolResult);

  # Shell execution (kaish REPL with block output)
  # Creates ToolCall (ToolKind::Shell) and ToolResult (ToolKind::Shell) blocks, streams output via BlockEvents
  shellExecute @28 (code :Text, documentId :Text, trace :TraceContext) -> (commandBlockId :BlockId);

  # Shell state (kaish working directory and last result)
  getCwd @29 () -> (path :Text);
  setCwd @30 (path :Text) -> (success :Bool, error :Text);
  getLastResult @31 () -> (result :ShellExecResult);

  # Blob storage (in-memory, size-capped)
  # Blobs are stored at /v/blobs/{id} in kaish's VFS
  writeBlob @32 (data :Data, contentType :Text) -> (ref :BlobRef);
  readBlob @33 (id :Text) -> (data :Data);
  deleteBlob @34 (id :Text) -> (success :Bool);
  listBlobs @35 () -> (refs :List(BlobRef));

  # Git repository management (CRDT-backed worktrees)
  registerRepo @36 (name :Text, path :Text) -> (success :Bool, error :Text);
  unregisterRepo @37 (name :Text) -> (success :Bool, error :Text);
  listRepos @38 () -> (repos :List(Text));
  switchBranch @39 (repo :Text, branch :Text) -> (success :Bool, error :Text);
  listBranches @40 (repo :Text) -> (branches :List(Text), error :Text);
  getCurrentBranch @41 (repo :Text) -> (branch :Text);
  flushGit @42 () -> (success :Bool, error :Text);
  setAttribution @43 (source :Text, command :Text);

  # Push CRDT operations from client to server for bidirectional sync
  # Returns ack version so client knows ops were accepted and ordered
  pushOps @44 (documentId :Text, ops :Data, trace :TraceContext) -> (ackVersion :UInt64);

  # MCP Resource management (push-first with caching)
  listMcpResources @45 (server :Text, trace :TraceContext) -> (resources :List(McpResource));
  readMcpResource @46 (server :Text, uri :Text, trace :TraceContext) -> (contents :McpResourceContents, hasContents :Bool);
  subscribeMcpResources @47 (callback :ResourceEvents);

  # MCP Roots (client advertises workspaces to servers)
  setMcpRoots @48 (roots :List(McpRoot));

  # MCP Prompts (poll-based with optional caching)
  listMcpPrompts @49 (server :Text) -> (prompts :List(McpPrompt));
  getMcpPrompt @50 (server :Text, name :Text, arguments :Text) -> (messages :List(McpPromptMessage));

  # MCP Progress (push-based streaming)
  subscribeMcpProgress @51 (callback :ProgressEvents);

  # MCP Elicitation (server-initiated requests for user input)
  subscribeMcpElicitations @52 (callback :ElicitationEvents);

  # MCP Completion (request/response)
  completeMcp @53 (server :Text, refType :Text, refName :Text, argName :Text, value :Text) -> (result :McpCompletionResult);

  # MCP Logging
  setMcpLogLevel @54 (server :Text, level :Text);
  subscribeMcpLogs @55 (callback :LoggingEvents);

  # MCP Cancellation
  cancelMcpRequest @56 (server :Text, requestId :Text);

  # ============================================================================
  # Agent Attachment
  # ============================================================================
  # Agents are autonomous participants that can edit content alongside humans.

  # Attach an agent to this kernel
  attachAgent @57 (config :AgentConfig) -> (info :AgentInfo);

  # List all attached agents on this kernel
  listAgents @58 () -> (agents :List(AgentInfo));

  # Detach an agent from this kernel
  detachAgent @59 (nick :Text);

  # Update agent capabilities
  setAgentCapabilities @60 (nick :Text, capabilities :List(AgentCapability));

  # Invoke an agent on a specific block (e.g., spell-check focused content)
  invokeAgent @61 (nick :Text, blockId :BlockId, action :Text) -> (requestId :Text);

  # Subscribe to agent activity events
  subscribeAgentEvents @62 (callback :AgentEvents);

  # ============================================================================
  # Timeline Navigation (Fork-First Temporal Model)
  # ============================================================================
  # Navigate through document history with fork and cherry-pick operations.
  # The past is read-only, but forking is ubiquitous.

  # Fork from a specific document version
  forkFromVersion @63 (documentId :Text, version :UInt64, contextLabel :Text, trace :TraceContext) -> (contextId :Data);

  # Cherry-pick a block into another context (carries lineage)
  cherryPickBlock @64 (sourceBlockId :BlockId, targetContextId :Data, trace :TraceContext) -> (newBlockId :BlockId);

  # Get document version history for timeline scrubber
  getDocumentHistory @65 (documentId :Text, limit :UInt32, trace :TraceContext) -> (snapshots :List(VersionSnapshot));

  # ============================================================================
  # Configuration (Config as CRDT)
  # ============================================================================
  # Config files (theme.rhai) are managed as CRDT documents.

  # List loaded config documents
  listConfigs @66 () -> (configs :List(Text));

  # Reload a config file from disk, discarding CRDT changes (safety valve)
  reloadConfig @67 (path :Text) -> (success :Bool, error :Text);

  # Reset a config file to embedded default
  resetConfig @68 (path :Text) -> (success :Bool, error :Text);

  # Get config content (from CRDT)
  getConfig @69 (path :Text) -> (content :Text, error :Text);

  # ============================================================================
  # Multi-Context Drifting (Cross-Context Communication)
  # ============================================================================
  # Drift enables content transfer between kernel contexts with provenance tracking.

  # Get this kernel's context ID and label
  getContextId @70 (trace :TraceContext) -> (id :Data, label :Text);

  # Configure LLM provider/model on an existing kernel
  configureLlm @71 (provider :Text, model :Text, trace :TraceContext) -> (success :Bool, error :Text);

  # Push content to another context's staging queue
  driftPush @72 (targetCtx :Data, content :Text, summarize :Bool, trace :TraceContext) -> (stagedId :UInt64);

  # Flush all staged drifts (inject into target kernels)
  driftFlush @73 (trace :TraceContext) -> (count :UInt32);

  # List staged drifts pending flush
  driftQueue @74 () -> (staged :List(StagedDriftInfo));

  # Cancel a staged drift by ID
  driftCancel @75 (stagedId :UInt64) -> (success :Bool);

  # Pull summarized content from another context
  driftPull @76 (sourceCtx :Data, prompt :Text, trace :TraceContext) -> (blockId :BlockId);

  # Merge a forked context back into its parent
  driftMerge @77 (sourceCtx :Data, trace :TraceContext) -> (blockId :BlockId);

  # Rename a context's label
  renameContext @78 (contextId :Data, label :Text);

  # ============================================================================
  # LLM Configuration (Per-Kernel Multi-Provider LLM)
  # ============================================================================

  # Get current LLM configuration for this kernel
  getLlmConfig @79 (trace :TraceContext) -> (config :LlmConfigInfo);

  # Set default LLM provider
  setDefaultProvider @80 (provider :Text) -> (success :Bool, error :Text);

  # Set default model for a provider
  setDefaultModel @81 (provider :Text, model :Text) -> (success :Bool, error :Text);

  # ============================================================================
  # Tool Filter Configuration
  # ============================================================================

  # Get current tool filter configuration
  getToolFilter @82 () -> (filter :ToolFilterConfig);

  # Set tool filter configuration
  setToolFilter @83 (filter :ToolFilterConfig) -> (success :Bool, error :Text);

  # ============================================================================
  # Shell Variable Introspection (kaish scope)
  # ============================================================================

  # Get a shell variable by name
  getShellVar @84 (name :Text) -> (value :ShellValue, found :Bool);

  # Set a shell variable
  setShellVar @85 (name :Text, value :ShellValue) -> (success :Bool, error :Text);

  # List all shell variables
  listShellVars @86 () -> (vars :List(ShellVar));

  # Compact a document's oplog, bumping sync generation.
  # Connected clients will receive onSyncReset and must re-fetch full state.
  compactDocument @87 (documentId :Text, trace :TraceContext) -> (newSize :UInt64, generation :UInt64);
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
