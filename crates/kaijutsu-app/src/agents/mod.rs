//! Agent module for collaborative editing.
//!
//! This module provides the client-side infrastructure for agent attachment:
//! - Tracking attached agents and their capabilities
//! - Visual indicators for agent presence and activity
//! - Key bindings for invoking agent capabilities
//!
//! # Architecture
//!
//! Agents are tracked via the server's agent registry. The client:
//! 1. Receives agent events via RPC subscription
//! 2. Updates local state (AgentRegistry resource)
//! 3. Renders indicators in the UI
//!
//! # Key Bindings (Processing Chain Triggers)
//!
//! - `Ctrl+S`: Invoke spell-check agent on focused block
//! - `Ctrl+R`: Invoke review agent on focused block
//! - `Ctrl+G`: Invoke generate agent on focused block

mod components;
mod plugin;
mod registry;
mod systems;

pub use components::*;
pub use plugin::AgentsPlugin;
pub use registry::*;
