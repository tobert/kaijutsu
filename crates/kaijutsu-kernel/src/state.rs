//! Kernel state: variables, history, checkpoints.
//!
//! This module manages the ephemeral state of a kernel, including
//! environment variables, command history, and checkpoints.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::SystemTime;
use uuid::Uuid;

/// Kernel state container.
///
/// Holds variables, history, and checkpoints. All state is in-memory
/// for now; persistence can be added later via SQLite.
#[derive(Debug, Default)]
pub struct KernelState {
    /// Kernel ID.
    pub id: Uuid,
    /// Human-readable name.
    pub name: String,
    /// Environment variables.
    vars: HashMap<String, String>,
    /// Command history.
    history: Vec<HistoryEntry>,
    /// Checkpoints.
    checkpoints: Vec<Checkpoint>,
    /// Next history ID.
    next_history_id: u64,
}

impl KernelState {
    /// Create a new kernel state with the given name.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: name.into(),
            vars: HashMap::new(),
            history: Vec::new(),
            checkpoints: Vec::new(),
            next_history_id: 1,
        }
    }

    /// Create a new kernel state with a specific ID.
    pub fn with_id(id: Uuid, name: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            vars: HashMap::new(),
            history: Vec::new(),
            checkpoints: Vec::new(),
            next_history_id: 1,
        }
    }

    // ========================================================================
    // Variables
    // ========================================================================

    /// Get a variable value.
    pub fn get_var(&self, name: &str) -> Option<&str> {
        self.vars.get(name).map(|s| s.as_str())
    }

    /// Set a variable value.
    pub fn set_var(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.vars.insert(name.into(), value.into());
    }

    /// Remove a variable.
    pub fn unset_var(&mut self, name: &str) -> Option<String> {
        self.vars.remove(name)
    }

    /// Get all variables.
    pub fn vars(&self) -> &HashMap<String, String> {
        &self.vars
    }

    /// Get mutable access to variables.
    pub fn vars_mut(&mut self) -> &mut HashMap<String, String> {
        &mut self.vars
    }

    // ========================================================================
    // History
    // ========================================================================

    /// Add a command to history.
    pub fn add_history(&mut self, command: impl Into<String>) -> u64 {
        let id = self.next_history_id;
        self.next_history_id += 1;

        self.history.push(HistoryEntry {
            id,
            command: command.into(),
            timestamp: SystemTime::now(),
            output: None,
            exit_code: None,
        });

        id
    }

    /// Add a command with its result to history.
    pub fn add_history_with_result(
        &mut self,
        command: impl Into<String>,
        output: impl Into<String>,
        exit_code: i32,
    ) -> u64 {
        let id = self.next_history_id;
        self.next_history_id += 1;

        self.history.push(HistoryEntry {
            id,
            command: command.into(),
            timestamp: SystemTime::now(),
            output: Some(output.into()),
            exit_code: Some(exit_code),
        });

        id
    }

    /// Update a history entry with its result.
    pub fn set_history_result(&mut self, id: u64, output: impl Into<String>, exit_code: i32) {
        if let Some(entry) = self.history.iter_mut().find(|e| e.id == id) {
            entry.output = Some(output.into());
            entry.exit_code = Some(exit_code);
        }
    }

    /// Get the last N history entries.
    pub fn recent_history(&self, limit: usize) -> &[HistoryEntry] {
        let start = self.history.len().saturating_sub(limit);
        &self.history[start..]
    }

    /// Get all history.
    pub fn history(&self) -> &[HistoryEntry] {
        &self.history
    }

    /// Get a specific history entry.
    pub fn get_history(&self, id: u64) -> Option<&HistoryEntry> {
        self.history.iter().find(|e| e.id == id)
    }

    /// Clear history.
    pub fn clear_history(&mut self) {
        self.history.clear();
    }

    // ========================================================================
    // Checkpoints
    // ========================================================================

    /// Create a checkpoint of the current state.
    pub fn checkpoint(&mut self, name: impl Into<String>) -> Uuid {
        let id = Uuid::new_v4();
        self.checkpoints.push(Checkpoint {
            id,
            name: name.into(),
            timestamp: SystemTime::now(),
            vars: self.vars.clone(),
            history_len: self.history.len(),
        });
        id
    }

    /// List all checkpoints.
    pub fn checkpoints(&self) -> &[Checkpoint] {
        &self.checkpoints
    }

    /// Get a checkpoint by ID.
    pub fn get_checkpoint(&self, id: Uuid) -> Option<&Checkpoint> {
        self.checkpoints.iter().find(|c| c.id == id)
    }

    /// Restore to a checkpoint.
    ///
    /// Restores variables and truncates history to the checkpoint's length.
    pub fn restore_checkpoint(&mut self, id: Uuid) -> bool {
        if let Some(checkpoint) = self.checkpoints.iter().find(|c| c.id == id).cloned() {
            self.vars = checkpoint.vars;
            self.history.truncate(checkpoint.history_len);
            true
        } else {
            false
        }
    }

    /// Delete a checkpoint.
    pub fn delete_checkpoint(&mut self, id: Uuid) -> bool {
        if let Some(pos) = self.checkpoints.iter().position(|c| c.id == id) {
            self.checkpoints.remove(pos);
            true
        } else {
            false
        }
    }

    // ========================================================================
    // Fork/Thread
    // ========================================================================

    /// Create a deep copy for forking.
    pub fn fork(&self, new_name: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: new_name.into(),
            vars: self.vars.clone(),
            history: self.history.clone(),
            checkpoints: Vec::new(), // Checkpoints don't carry over
            next_history_id: self.next_history_id,
        }
    }

    /// Create a lightweight copy for threading.
    ///
    /// Shares the same ID lineage but starts fresh.
    pub fn thread(&self, new_name: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: new_name.into(),
            vars: self.vars.clone(), // Inherit vars
            history: Vec::new(),     // Fresh history
            checkpoints: Vec::new(),
            next_history_id: 1,
        }
    }
}

/// A command history entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// Unique ID for this entry.
    pub id: u64,
    /// The command that was executed.
    pub command: String,
    /// When the command was executed.
    pub timestamp: SystemTime,
    /// Output from the command (if captured).
    pub output: Option<String>,
    /// Exit code (if completed).
    pub exit_code: Option<i32>,
}

/// A checkpoint of kernel state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    /// Unique ID for this checkpoint.
    pub id: Uuid,
    /// Human-readable name.
    pub name: String,
    /// When the checkpoint was created.
    pub timestamp: SystemTime,
    /// Variables at checkpoint time.
    pub vars: HashMap<String, String>,
    /// Length of history at checkpoint time.
    pub history_len: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_variables() {
        let mut state = KernelState::new("test");

        state.set_var("FOO", "bar");
        assert_eq!(state.get_var("FOO"), Some("bar"));

        state.set_var("FOO", "baz");
        assert_eq!(state.get_var("FOO"), Some("baz"));

        state.unset_var("FOO");
        assert_eq!(state.get_var("FOO"), None);
    }

    #[test]
    fn test_history() {
        let mut state = KernelState::new("test");

        let id1 = state.add_history("echo hello");
        let id2 = state.add_history("ls -la");

        assert_eq!(state.history().len(), 2);
        assert_eq!(state.get_history(id1).unwrap().command, "echo hello");
        assert_eq!(state.get_history(id2).unwrap().command, "ls -la");

        let recent = state.recent_history(1);
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].command, "ls -la");
    }

    #[test]
    fn test_history_with_result() {
        let mut state = KernelState::new("test");

        let id = state.add_history("echo hello");
        state.set_history_result(id, "hello\n", 0);

        let entry = state.get_history(id).unwrap();
        assert_eq!(entry.output, Some("hello\n".to_string()));
        assert_eq!(entry.exit_code, Some(0));
    }

    #[test]
    fn test_checkpoint() {
        let mut state = KernelState::new("test");

        state.set_var("X", "1");
        state.add_history("cmd1");

        let cp_id = state.checkpoint("before change");

        state.set_var("X", "2");
        state.add_history("cmd2");

        assert_eq!(state.get_var("X"), Some("2"));
        assert_eq!(state.history().len(), 2);

        state.restore_checkpoint(cp_id);

        assert_eq!(state.get_var("X"), Some("1"));
        assert_eq!(state.history().len(), 1);
    }

    #[test]
    fn test_fork() {
        let mut state = KernelState::new("parent");
        state.set_var("FOO", "bar");
        state.add_history("cmd1");

        let forked = state.fork("child");

        assert_ne!(forked.id, state.id);
        assert_eq!(forked.name, "child");
        assert_eq!(forked.get_var("FOO"), Some("bar"));
        assert_eq!(forked.history().len(), 1);
    }

    #[test]
    fn test_thread() {
        let mut state = KernelState::new("parent");
        state.set_var("FOO", "bar");
        state.add_history("cmd1");

        let threaded = state.thread("worker");

        assert_ne!(threaded.id, state.id);
        assert_eq!(threaded.name, "worker");
        assert_eq!(threaded.get_var("FOO"), Some("bar")); // Inherits vars
        assert_eq!(threaded.history().len(), 0); // Fresh history
    }
}
