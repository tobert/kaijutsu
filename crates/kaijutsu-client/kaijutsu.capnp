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

  # Future: VFS mount/unmount, Lease acquire/release, Checkpoint
}
