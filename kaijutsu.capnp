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
  # Future: mounts, lease status, consent mode
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
  cellId @2 :Text;        # Target cell for response blocks
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
  applyBlockOp @18 (cellId :Text, op :BlockDocOp) -> (newVersion :UInt64);
  subscribeBlocks @19 (callback :BlockEvents);
  getBlockCellState @20 (cellId :Text) -> (state :BlockCellState);

  # LLM operations
  prompt @21 (request :LlmRequest) -> (promptId :Text);
}

# ============================================================================
# Block-Based CRDT Types (DAG Architecture)
# ============================================================================

# Block identifier - globally unique across all cells and agents
struct BlockId {
  cellId @0 :Text;
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

# Full cell state with blocks
struct BlockCellState {
  cellId @0 :Text;
  blocks @1 :List(BlockSnapshot);
  version @2 :UInt64;
}

# Callback for receiving block updates from server
interface BlockEvents {
  onBlockInserted @0 (cellId :Text, block :BlockSnapshot, afterId :BlockId, hasAfterId :Bool);
  onBlockDeleted @1 (cellId :Text, blockId :BlockId);
  onBlockEdited @2 (cellId :Text, blockId :BlockId, pos :UInt64, insert :Text, delete :UInt64);
  onBlockCollapsed @3 (cellId :Text, blockId :BlockId, collapsed :Bool);
  onBlockMoved @4 (cellId :Text, blockId :BlockId, afterId :BlockId, hasAfterId :Bool);
  onBlockStatusChanged @5 (cellId :Text, blockId :BlockId, status :Status);
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
