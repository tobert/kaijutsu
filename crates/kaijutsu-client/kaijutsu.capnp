@0xb8e3f4a9c2d1e0f7;

# Kaijutsu RPC Schema
# Cap'n Proto interface for sshwarma server communication

# ============================================================================
# Core Types
# ============================================================================

struct Identity {
  username @0 :Text;
  displayName @1 :Text;
}

struct RoomInfo {
  id @0 :UInt64;
  name @1 :Text;
  branch @2 :Text;
  userCount @3 :UInt32;
  agentCount @4 :UInt32;
}

struct RoomConfig {
  name @0 :Text;
  branch @1 :Text;              # Optional branch stem suggestion
  repos @2 :List(RepoMount);
}

struct RepoMount {
  name @0 :Text;                # e.g. "kaijutsu"
  url @1 :Text;                 # e.g. "git@github.com:atobey/kaijutsu.git"
  writable @2 :Bool;            # rw vs ro
}

struct ToolInfo {
  name @0 :Text;
  description @1 :Text;
  equipped @2 :Bool;
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

interface RoomEvents {
  onRow @0 (row :Row);
  onUserJoin @1 (username :Text);
  onUserLeave @2 (username :Text);
  onAgentStatus @3 (agent :Text, status :Text);
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

  # Room management
  listRooms @1 () -> (rooms :List(RoomInfo));
  joinRoom @2 (name :Text) -> (room :Room);
  createRoom @3 (config :RoomConfig) -> (room :Room);
}

interface Room {
  # Info
  getInfo @0 () -> (info :RoomInfo);
  getHistory @1 (limit :UInt32, beforeId :UInt64) -> (rows :List(Row));

  # Messaging
  send @2 (content :Text) -> (row :Row);
  mention @3 (agent :Text, content :Text) -> (row :Row);
  subscribe @4 (callback :RoomEvents);

  # kaish kernel (shared per room)
  getKernel @5 () -> (kernel :KaishKernel);

  # Equipment
  listEquipment @6 () -> (tools :List(ToolInfo));
  equip @7 (tool :Text);
  unequip @8 (tool :Text);

  # Room management
  fork @9 (newName :Text) -> (room :Room);
  leave @10 ();
}

interface KaishKernel {
  execute @0 (code :Text) -> (execId :UInt64);
  interrupt @1 (execId :UInt64);
  complete @2 (partial :Text, cursor :UInt32) -> (completions :List(Completion));
  subscribe @3 (callback :KernelOutput);
  getHistory @4 (limit :UInt32) -> (entries :List(HistoryEntry));
}
