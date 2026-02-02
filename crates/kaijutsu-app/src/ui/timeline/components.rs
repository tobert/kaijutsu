//! Timeline ECS components and resources.

#![allow(dead_code)] // Phase 3 infrastructure - not all used yet

use bevy::prelude::*;
use kaijutsu_crdt::BlockId;

// ============================================================================
// TIMELINE STATE
// ============================================================================

/// How the timeline is being viewed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Reflect)]
pub enum TimelineViewMode {
    /// Following live - showing current state as it evolves.
    #[default]
    Live,
    /// Viewing a historical snapshot - past is read-only.
    Historical,
}

/// Timeline position within conversation history.
///
/// The timeline represents the conversation's evolution through CRDT operations.
/// Position 0.0 is the beginning (empty document), 1.0 is the current state.
#[derive(Resource, Reflect)]
#[reflect(Resource)]
pub struct TimelineState {
    /// Current viewing position (0.0 = origin, 1.0 = current).
    pub position: f32,
    /// Target position for smooth scrubbing.
    pub target_position: f32,
    /// Current viewing mode.
    pub mode: TimelineViewMode,
    /// Document version at the current viewing position.
    /// Used to fetch the appropriate snapshot.
    pub viewing_version: u64,
    /// The document's current (latest) version.
    pub current_version: u64,
    /// Number of significant snapshots (operations that change visible content).
    pub snapshot_count: u64,
    /// Whether the timeline scrubber is currently being dragged.
    pub scrubbing: bool,
    /// Whether the timeline UI is expanded (visible).
    pub expanded: bool,
}

impl Default for TimelineState {
    fn default() -> Self {
        Self {
            position: 1.0,
            target_position: 1.0,
            mode: TimelineViewMode::Live,
            viewing_version: 0,
            current_version: 0,
            snapshot_count: 0,
            scrubbing: false,
            expanded: true,
        }
    }
}

impl TimelineState {
    /// Check if viewing the current/live state.
    pub fn is_live(&self) -> bool {
        matches!(self.mode, TimelineViewMode::Live)
    }

    /// Check if viewing historical content.
    pub fn is_historical(&self) -> bool {
        matches!(self.mode, TimelineViewMode::Historical)
    }

    /// Jump to live (current) state.
    pub fn jump_to_live(&mut self) {
        self.target_position = 1.0;
        self.position = 1.0;
        self.mode = TimelineViewMode::Live;
        self.viewing_version = self.current_version;
    }

    /// Start scrubbing at a position.
    pub fn begin_scrub(&mut self, position: f32) {
        self.scrubbing = true;
        self.target_position = position.clamp(0.0, 1.0);
        if self.target_position < 1.0 {
            self.mode = TimelineViewMode::Historical;
        }
    }

    /// Update scrub position.
    pub fn update_scrub(&mut self, position: f32) {
        if self.scrubbing {
            self.target_position = position.clamp(0.0, 1.0);
            self.position = self.target_position; // Instant for now

            // Calculate viewing version from position
            if self.snapshot_count > 0 {
                self.viewing_version = ((self.target_position * self.snapshot_count as f32) as u64)
                    .min(self.current_version);
            }

            self.mode = if self.target_position >= 1.0 {
                TimelineViewMode::Live
            } else {
                TimelineViewMode::Historical
            };
        }
    }

    /// End scrubbing.
    pub fn end_scrub(&mut self) {
        self.scrubbing = false;
        // Snap to nearest significant snapshot
        // For now, just stay where we are
    }

    /// Update when document version changes.
    pub fn sync_version(&mut self, new_version: u64) {
        self.current_version = new_version;
        self.snapshot_count = new_version; // 1:1 for now

        // If in live mode, stay at current
        if self.is_live() {
            self.viewing_version = new_version;
            self.position = 1.0;
            self.target_position = 1.0;
        }
    }
}

// ============================================================================
// FORK/CHERRY-PICK SUPPORT
// ============================================================================

/// Request to fork the current context from a specific point.
///
/// When triggered, this creates a new context that branches from the
/// timeline position where the fork was requested.
#[derive(Message, Debug, Clone)]
pub struct ForkRequest {
    /// The version to fork from (0 = fork from current viewing position).
    pub from_version: u64,
    /// Name for the new forked context (optional, auto-generated if None).
    pub name: Option<String>,
}

/// Request to cherry-pick a block into another context.
///
/// Cherry-picked blocks carry their lineage - the history of how they
/// were created, enabling "why did we decide X?" queries.
#[derive(Message, Debug, Clone)]
pub struct CherryPickRequest {
    /// The block to cherry-pick.
    pub block_id: BlockId,
    /// Target context to pick into.
    pub target_context: String,
}

/// Result of a fork operation.
#[derive(Message, Debug, Clone)]
pub struct ForkResult {
    /// Whether the fork succeeded.
    pub success: bool,
    /// New context ID if successful.
    pub context_id: Option<String>,
    /// Error message if failed.
    pub error: Option<String>,
}

/// Result of a cherry-pick operation.
#[derive(Message, Debug, Clone)]
pub struct CherryPickResult {
    /// Whether the pick succeeded.
    pub success: bool,
    /// New block ID in target context.
    pub new_block_id: Option<BlockId>,
    /// Error message if failed.
    pub error: Option<String>,
}

// ============================================================================
// UI MARKERS
// ============================================================================

/// Marker for the timeline scrubber container.
#[derive(Component)]
pub struct TimelineScrubber;

/// Marker for the timeline track (the draggable area).
#[derive(Component)]
pub struct TimelineTrack;

/// Marker for the timeline position indicator (the "playhead").
#[derive(Component)]
pub struct TimelinePlayhead;

/// Marker for the "filled" portion of the timeline (before playhead).
#[derive(Component)]
pub struct TimelineFill;

/// Marker for the "Fork from here" button.
#[derive(Component)]
pub struct ForkButton;

/// Marker for the "Jump to Now" button.
#[derive(Component)]
pub struct JumpToNowButton;

/// Marker for a timeline tick mark (visual reference point).
#[derive(Component)]
pub struct TimelineTick {
    /// Position along the timeline (0.0 to 1.0).
    pub position: f32,
    /// The version this tick represents.
    pub version: u64,
}

// ============================================================================
// BLOCK VISUAL STATE
// ============================================================================

/// Visual modifier for blocks based on timeline position.
///
/// Blocks in the "past" (before viewing position) appear dimmed.
/// Blocks at or after viewing position appear normal.
#[derive(Component, Debug, Clone, Reflect)]
#[reflect(Component)]
pub struct TimelineVisibility {
    /// The version when this block was created.
    pub created_at_version: u64,
    /// Current opacity (0.0 = hidden, 1.0 = fully visible).
    pub opacity: f32,
    /// Whether this block is in the "past" relative to viewing position.
    pub is_past: bool,
}

impl Default for TimelineVisibility {
    fn default() -> Self {
        Self {
            created_at_version: 0,
            opacity: 1.0,
            is_past: false,
        }
    }
}
