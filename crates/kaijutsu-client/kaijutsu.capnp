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

struct CellOp {
  cellId @0 :Text;
  clientVersion @1 :UInt64;   # Client's version before this op
  op @2 :CrdtOp;
}

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

struct CellState {
  info @0 :CellInfo;
  content @1 :Text;           # Current text content
  version @2 :UInt64;         # Server's CRDT version
  encodedDoc @3 :Data;        # Optional: full diamond-types doc for sync
}

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
}
