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
# Message DAG Types
# ============================================================================

struct Row {
  id @0 :UInt64;
  parentId @1 :UInt64;          # 0 = root level
  rowType @2 :RowType;
  sender @3 :Text;
  content @4 :Text;
  timestamp @5 :UInt64;
  metadata @6 :Text;            # JSON for extensibility
}

enum RowType {
  chat @0;
  agentResponse @1;
  toolCall @2;
  toolResult @3;
  systemMessage @4;
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

interface KernelEvents {
  onRow @0 (row :Row);
  onUserJoin @1 (username :Text);
  onUserLeave @2 (username :Text);
  onAgentStatus @3 (agent :Text, status :Text);
  # Future: onLeaseChange, onCheckpoint
}

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
  getHistory @1 (limit :UInt32, beforeId :UInt64) -> (rows :List(Row));

  # Messaging
  send @2 (content :Text) -> (row :Row);
  mention @3 (agent :Text, content :Text) -> (row :Row);
  subscribe @4 (callback :KernelEvents);

  # kaish execution (directly on Kernel, no indirection)
  execute @5 (code :Text) -> (execId :UInt64);
  interrupt @6 (execId :UInt64);
  complete @7 (partial :Text, cursor :UInt32) -> (completions :List(Completion));
  subscribeOutput @8 (callback :KernelOutput);
  getCommandHistory @9 (limit :UInt32) -> (entries :List(HistoryEntry));

  # Equipment
  listEquipment @10 () -> (tools :List(ToolInfo));
  equip @11 (tool :Text);
  unequip @12 (tool :Text);

  # Lifecycle
  fork @13 (name :Text) -> (kernel :Kernel);
  thread @14 (name :Text) -> (kernel :Kernel);
  detach @15 ();

  # VFS
  vfs @16 () -> (vfs :Vfs);
  listMounts @17 () -> (mounts :List(MountInfo));
  mount @18 (path :Text, source :Text, writable :Bool);
  unmount @19 (path :Text) -> (success :Bool);

  # Cell CRDT sync
  listCells @20 () -> (cells :List(CellInfo));
  getCell @21 (cellId :Text) -> (cell :CellState);
  createCell @22 (kind :CellKind, language :Text, parentId :Text) -> (cell :CellState);
  deleteCell @23 (cellId :Text);
  applyOp @24 (op :CellOp) -> (newVersion :UInt64);
  subscribeCells @25 (callback :CellEvents);

  # Bulk sync for initial load / reconnect
  syncCells @26 (fromVersions :List(CellVersion)) -> (patches :List(CellPatch), newCells :List(CellState));

  # Tool execution
  executeTool @27 (call :ToolCall) -> (result :ToolResult);
  getToolSchemas @28 () -> (schemas :List(ToolSchema));

  # Block-based CRDT operations (new architecture)
  applyBlockOp @29 (cellId :Text, op :BlockDocOp) -> (newVersion :UInt64);
  subscribeBlocks @30 (callback :BlockEvents);
  getBlockCellState @31 (cellId :Text) -> (state :BlockCellState);

  # LLM operations
  prompt @32 (request :LlmRequest) -> (promptId :Text);
}

struct CellVersion {
  cellId @0 :Text;
  version @1 :UInt64;
}

# ============================================================================
# Cell CRDT Types
# ============================================================================

enum CellKind {
  code @0;
  markdown @1;
  output @2;
  system @3;
  userMessage @4;
  agentMessage @5;
}

struct CellInfo {
  id @0 :Text;
  kind @1 :CellKind;
  language @2 :Text;          # For code cells, e.g. "rust", "python"
  parentId @3 :Text;          # For nested cells (tool calls under messages)
}

# Legacy CellOp - kept for transitional sync (will be removed)
struct CellOp {
  cellId @0 :Text;
  clientVersion @1 :UInt64;   # Client's version before this op
  op @2 :CrdtOp;
}

# Legacy CrdtOp - kept for transitional sync (will be removed)
struct CrdtOp {
  union {
    insert :group {
      pos @0 :UInt64;
      text @1 :Text;
    }
    delete :group {
      pos @2 :UInt64;
      len @3 :UInt64;
    }
    # Full state sync (for initial load or recovery)
    fullState @4 :Data;       # Encoded diamond-types document
  }
}

# Legacy CellState - kept for transitional sync
struct CellState {
  info @0 :CellInfo;
  content @1 :Text;           # Current text content
  version @2 :UInt64;         # Server's CRDT version
  encodedDoc @3 :Data;        # Optional: full diamond-types doc for sync
}

# Legacy CellPatch - kept for transitional sync
struct CellPatch {
  cellId @0 :Text;
  fromVersion @1 :UInt64;     # Version patch is based on
  toVersion @2 :UInt64;       # Version after applying patch
  ops @3 :Data;               # Encoded diamond-types patch
}

# Callback for receiving cell updates from server
interface CellEvents {
  onCellCreated @0 (cell :CellState);
  onCellUpdated @1 (patch :CellPatch);
  onCellDeleted @2 (cellId :Text);
}

# ============================================================================
# Block-Based CRDT Types (New Architecture)
# ============================================================================

# Block identifier - globally unique across all cells and agents
struct BlockId {
  cellId @0 :Text;
  agentId @1 :Text;
  seq @2 :UInt64;
}

# Block content types - supports structured response blocks
struct BlockContent {
  union {
    # Extended thinking - text CRDT, collapsible
    thinking :group {
      text @0 :Text;
      collapsed @1 :Bool;
    }
    # Main response text - text CRDT
    text @2 :Text;
    # Tool invocation - immutable after creation
    toolUse :group {
      id @3 :Text;
      name @4 :Text;
      input @5 :Text;         # JSON encoded
    }
    # Tool result - immutable after creation
    toolResult :group {
      toolUseId @6 :Text;
      content @7 :Text;
      isError @8 :Bool;
    }
  }
}

# Operations on block documents
struct BlockDocOp {
  union {
    # Insert a new block after a reference block
    insertBlock :group {
      id @0 :BlockId;
      afterId @1 :BlockId;
      hasAfterId @2 :Bool;    # False = insert at start
      content @3 :BlockContent;
      author @14 :Text;       # Who created this block (e.g. "user:amy", "model:claude")
      createdAt @15 :UInt64;  # Unix timestamp in milliseconds
    }
    # Delete a block
    deleteBlock @4 :BlockId;
    # Edit text within a block (Thinking/Text only)
    editBlockText :group {
      id @5 :BlockId;
      pos @6 :UInt64;
      insert @7 :Text;
      delete @8 :UInt64;
    }
    # Toggle collapsed state (Thinking only)
    setCollapsed :group {
      id @9 :BlockId;
      collapsed @10 :Bool;
    }
    # Move block to new position
    moveBlock :group {
      id @11 :BlockId;
      afterId @12 :BlockId;
      hasAfterId @13 :Bool;
    }
  }
}

# State of a single block
struct BlockState {
  id @0 :BlockId;
  content @1 :BlockContent;
  textVersion @2 :UInt64;     # Version of text CRDT (for Thinking/Text)
  author @3 :Text;            # Who created this block
  createdAt @4 :UInt64;       # Unix timestamp in milliseconds
}

# Full cell state with blocks (new architecture)
struct BlockCellState {
  info @0 :CellInfo;
  blocks @1 :List(BlockState);
  version @2 :UInt64;
}

# Callback for receiving block updates from server
interface BlockEvents {
  onBlockInserted @0 (cellId :Text, block :BlockState, afterId :BlockId, hasAfterId :Bool);
  onBlockDeleted @1 (cellId :Text, blockId :BlockId);
  onBlockEdited @2 (cellId :Text, blockId :BlockId, pos :UInt64, insert :Text, delete :UInt64);
  onBlockCollapsed @3 (cellId :Text, blockId :BlockId, collapsed :Bool);
  onBlockMoved @4 (cellId :Text, blockId :BlockId, afterId :BlockId, hasAfterId :Bool);
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
