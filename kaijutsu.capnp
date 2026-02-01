@0xb8e3f4a9c2d1e0f7;

# Kaijutsu RPC Schema
# Cap'n Proto interface for kernel-based collaborative spaces

# ============================================================================
# Core Types
# ============================================================================

struct Identity {
  username @0 :Text;
  displayName @1 :Text;
}

struct KernelInfo {
  id @0 :Text;
  name @1 :Text;
  userCount @2 :UInt32;
  agentCount @3 :UInt32;
  contexts @4 :List(Context);       # Contexts within this kernel
  seatCount @5 :UInt32;             # Total seats across all contexts
}

# ============================================================================
# Seat and Context Types (Lobby Experience)
# ============================================================================

# Seat identifier - the 4-tuple that identifies a seat
struct SeatId {
  nick @0 :Text;          # Display name, user-chosen: "amy", "refactor-bot"
  instance @1 :Text;      # Device/model variant: "laptop", "haiku"
  kernel @2 :Text;        # Kernel name: "kaijutsu-dev"
  context @3 :Text;       # Context name: "refactor"
}

# Seat status
enum SeatStatus {
  active @0;              # Currently engaged
  idle @1;                # Connected but not actively working
  away @2;                # Stepped away
}

# Information about a seat
struct SeatInfo {
  id @0 :SeatId;
  owner @1 :Text;                   # Strong identity: username from SSH auth
  status @2 :SeatStatus;
  lastActivity @3 :UInt64;          # Unix timestamp ms
  cursorBlock @4 :Text;             # Which block they're focused on
}

# A document attached to a context
struct ContextDocument {
  id @0 :Text;                      # Block/document ID
  attachedBy @1 :Text;              # Who attached it (seat nick:instance)
  attachedAt @2 :UInt64;            # When it was attached (unix timestamp ms)
}

# A context within a kernel
struct Context {
  name @0 :Text;
  documents @1 :List(ContextDocument);
  seats @2 :List(SeatInfo);
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

struct ToolInfo {
  name @0 :Text;
  description @1 :Text;
  equipped @2 :Bool;
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

# Reference to binary data in blob storage (/v/blobs/{id})
struct BlobRef {
  id @0 :Text;              # Unique blob identifier
  size @1 :UInt64;          # Size in bytes
  contentType @2 :Text;     # MIME type (e.g., "image/png")
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

struct MountInfo {
  path @0 :Text;
  readOnly @1 :Bool;
}

# ============================================================================
# Kernel Output Types
# ============================================================================

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

# ============================================================================
# Event Callbacks (Client implements these)
# ============================================================================

interface KernelOutput {
  onOutput @0 (event :KernelOutputEvent);
}

# ============================================================================
# Server Interfaces
# ============================================================================

interface World {
  # Identity
  whoami @0 () -> (identity :Identity);

  # Kernel management
  listKernels @1 () -> (kernels :List(KernelInfo));
  attachKernel @2 (id :Text) -> (kernel :Kernel);
  createKernel @3 (config :KernelConfig) -> (kernel :Kernel);

  # Seat management (cross-kernel)
  takeSeat @4 (seat :SeatId) -> (handle :SeatHandle);
  listMySeats @5 () -> (seats :List(SeatInfo));
}

interface Kernel {
  # Info
  getInfo @0 () -> (info :KernelInfo);

  # kaish execution
  execute @1 (code :Text) -> (execId :UInt64);
  interrupt @2 (execId :UInt64);
  complete @3 (partial :Text, cursor :UInt32) -> (completions :List(Completion));
  subscribeOutput @4 (callback :KernelOutput);
  getCommandHistory @5 (limit :UInt32) -> (entries :List(HistoryEntry));

  # Equipment
  listEquipment @6 () -> (tools :List(ToolInfo));
  equip @7 (tool :Text);
  unequip @8 (tool :Text);

  # Lifecycle
  fork @9 (name :Text) -> (kernel :Kernel);
  thread @10 (name :Text) -> (kernel :Kernel);
  detach @11 ();

  # VFS
  vfs @12 () -> (vfs :Vfs);
  listMounts @13 () -> (mounts :List(MountInfo));
  mount @14 (path :Text, source :Text, writable :Bool);
  unmount @15 (path :Text) -> (success :Bool);

  # Tool execution
  executeTool @16 (call :ToolCall) -> (result :ToolResult);
  getToolSchemas @17 () -> (schemas :List(ToolSchema));

  # Block-based CRDT operations
  applyBlockOp @18 (documentId :Text, op :BlockDocOp) -> (newVersion :UInt64);
  subscribeBlocks @19 (callback :BlockEvents);
  getDocumentState @20 (documentId :Text) -> (state :DocumentState);

  # LLM operations
  prompt @21 (request :LlmRequest) -> (promptId :Text);

  # Context & seat management
  listContexts @22 () -> (contexts :List(Context));
  createContext @23 (name :Text) -> (context :Context);
  joinContext @24 (contextName :Text, instance :Text) -> (seat :SeatHandle);
  attachDocument @25 (contextName :Text, documentId :Text);
  detachDocument @26 (contextName :Text, documentId :Text);

  # MCP (Model Context Protocol) management
  registerMcp @27 (config :McpServerConfig) -> (info :McpServerInfo);
  unregisterMcp @28 (name :Text);
  listMcpServers @29 () -> (servers :List(McpServerInfo));
  callMcpTool @30 (call :McpToolCall) -> (result :McpToolResult);

  # Shell execution (kaish REPL with block output)
  # Creates ShellCommand and ShellOutput blocks, streams output via BlockEvents
  shellExecute @31 (code :Text, documentId :Text) -> (commandBlockId :BlockId);

  # Shell state (kaish working directory and last result)
  getCwd @32 () -> (path :Text);
  setCwd @33 (path :Text) -> (success :Bool, error :Text);
  getLastResult @34 () -> (result :ShellExecResult);

  # Blob storage (in-memory, size-capped)
  # Blobs are stored at /v/blobs/{id} in kaish's VFS
  writeBlob @35 (data :Data, contentType :Text) -> (ref :BlobRef);
  readBlob @36 (id :Text) -> (data :Data);
  deleteBlob @37 (id :Text) -> (success :Bool);
  listBlobs @38 () -> (refs :List(BlobRef));

  # Git repository management (CRDT-backed worktrees)
  registerRepo @39 (name :Text, path :Text) -> (success :Bool, error :Text);
  unregisterRepo @40 (name :Text) -> (success :Bool, error :Text);
  listRepos @41 () -> (repos :List(Text));
  switchBranch @42 (repo :Text, branch :Text) -> (success :Bool, error :Text);
  listBranches @43 (repo :Text) -> (branches :List(Text), error :Text);
  getCurrentBranch @44 (repo :Text) -> (branch :Text);
  flushGit @45 () -> (success :Bool, error :Text);
  setAttribution @46 (source :Text, command :Text);

  # Push CRDT operations from client to server for bidirectional sync
  # Returns ack version so client knows ops were accepted and ordered
  pushOps @47 (documentId :Text, ops :Data) -> (ackVersion :UInt64);

  # MCP Resource management (push-first with caching)
  listMcpResources @48 (server :Text) -> (resources :List(McpResource));
  readMcpResource @49 (server :Text, uri :Text) -> (contents :McpResourceContents, hasContents :Bool);
  subscribeMcpResources @50 (callback :ResourceEvents);

  # MCP Roots (client advertises workspaces to servers)
  setMcpRoots @51 (roots :List(McpRoot));

  # MCP Prompts (poll-based with optional caching)
  listMcpPrompts @52 (server :Text) -> (prompts :List(McpPrompt));
  getMcpPrompt @53 (server :Text, name :Text, arguments :Text) -> (messages :List(McpPromptMessage));

  # MCP Progress (push-based streaming)
  subscribeMcpProgress @54 (callback :ProgressEvents);

  # MCP Elicitation (server-initiated requests for user input)
  subscribeMcpElicitations @55 (callback :ElicitationEvents);

  # MCP Completion (request/response)
  completeMcp @56 (server :Text, refType :Text, refName :Text, argName :Text, value :Text) -> (result :McpCompletionResult);

  # MCP Logging
  setMcpLogLevel @57 (server :Text, level :Text);
  subscribeMcpLogs @58 (callback :LoggingEvents);

  # MCP Cancellation
  cancelMcpRequest @59 (server :Text, requestId :Text);
}

# Capability for interacting with a seat
interface SeatHandle {
  # Get current seat state
  getState @0 () -> (info :SeatInfo);

  # Update cursor position (which block we're focused on)
  updateCursor @1 (blockId :Text);

  # Set our status
  setStatus @2 (status :SeatStatus);

  # Leave the seat ("get up from the meeting")
  leave @3 ();
}

# ============================================================================
# Block-Based CRDT Types (DAG Architecture)
# ============================================================================

# Block identifier - globally unique across all documents and agents
struct BlockId {
  documentId @0 :Text;
  agentId @1 :Text;
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

# Block content type
enum BlockKind {
  text @0;
  thinking @1;
  toolCall @2;
  toolResult @3;
  shellCommand @4;
  shellOutput @5;
}

# Flat block snapshot - all fields present, some unused depending on kind
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
  author @6 :Text;
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

# Callback for receiving block updates from server
interface BlockEvents {
  onBlockInserted @0 (documentId :Text, block :BlockSnapshot, afterId :BlockId, hasAfterId :Bool, ops :Data);
  onBlockDeleted @1 (documentId :Text, blockId :BlockId);
  onBlockCollapsed @2 (documentId :Text, blockId :BlockId, collapsed :Bool);
  onBlockMoved @3 (documentId :Text, blockId :BlockId, afterId :BlockId, hasAfterId :Bool);
  onBlockStatusChanged @4 (documentId :Text, blockId :BlockId, status :Status);
  onBlockTextOps @5 (documentId :Text, blockId :BlockId, ops :Data);
}

# ============================================================================
# VFS Interface
# ============================================================================

# ============================================================================
# MCP (Model Context Protocol) Types
# ============================================================================

struct McpServerConfig {
  name @0 :Text;              # Unique name for this server (e.g., "git", "exa")
  command @1 :Text;           # Command to run (e.g., "uvx", "npx")
  args @2 :List(Text);        # Arguments for the command
  env @3 :List(EnvVar);       # Environment variables
  cwd @4 :Text;               # Working directory (optional)
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

# MCP Resource Types

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

# MCP Root - workspace directory advertised to servers
struct McpRoot {
  uri @0 :Text;              # file:// URI for the root
  name @1 :Text;             # Optional display name
  hasName @2 :Bool;
}

# MCP Prompt - reusable prompt template from a server
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

# MCP Progress - updates during long-running operations
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

# MCP Elicitation - server requests user input
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

# MCP Completion - argument value suggestions
struct McpCompletionResult {
  values @0 :List(Text);     # Suggested completions
  total @1 :UInt32;          # Total available if known
  hasTotal @2 :Bool;
}

# MCP Logging - log messages from servers
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
