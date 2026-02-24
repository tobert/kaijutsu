//! CRDT-backed input document per context.
//!
//! Each context gets a companion DTE text document for its compose input.
//! Any participant (user, agent, tool) can read and write it. Submission
//! snapshots the text into a conversation block and clears the input doc.
//!
//! This is deliberately NOT a BlockStore — no per-block overhead, no DAG,
//! no ordering keys. A single mutable text buffer is the right primitive.

use std::sync::atomic::{AtomicU64, Ordering};

use diamond_types_extended::{AgentId, Document, Frontier, SerializedOpsOwned, Uuid};
use parking_lot::RwLock;

use kaijutsu_types::PrincipalId;

/// A single context's input document (compose scratchpad).
///
/// Wraps a DTE Document with a single root-level text field ("input").
/// Concurrent edits from multiple participants merge automatically.
pub struct InputDocEntry {
    /// DTE Document holding the text content.
    doc: Document,
    /// DTE agent ID for this server replica.
    agent: AgentId,
    /// Monotonic version counter (bumped on every edit).
    version: AtomicU64,
    /// Last principal to modify the input.
    last_agent: RwLock<PrincipalId>,
}

/// Key used for the text field in the DTE Document root map.
const INPUT_TEXT_KEY: &str = "input";

impl InputDocEntry {
    /// Create a new empty input document.
    pub fn new(principal_id: PrincipalId) -> Self {
        let mut doc = Document::new();
        let agent = doc.create_agent(Uuid::from_bytes(*principal_id.as_bytes()));

        // Create the root text field via writer
        let mut w = doc.writer(agent);
        w.root_create_text(INPUT_TEXT_KEY);

        Self {
            doc,
            agent,
            version: AtomicU64::new(0),
            last_agent: RwLock::new(principal_id),
        }
    }

    /// Create an input document from serialized ops (for DB restore).
    pub fn from_ops(ops_bytes: &[u8], principal_id: PrincipalId) -> Result<Self, String> {
        let ops: SerializedOpsOwned = postcard::from_bytes(ops_bytes)
            .map_err(|e| format!("Failed to deserialize input doc ops: {}", e))?;

        let mut doc = Document::new();
        let agent = doc.create_agent(Uuid::from_bytes(*principal_id.as_bytes()));
        doc.merge_ops(ops)
            .map_err(|e| format!("Failed to merge input doc ops: {}", e))?;

        let version = doc.version().len() as u64;

        Ok(Self {
            doc,
            agent,
            version: AtomicU64::new(version),
            last_agent: RwLock::new(principal_id),
        })
    }

    /// Edit the input text at a position.
    ///
    /// Inserts `insert` text at `pos` and deletes `delete` characters starting at `pos`.
    /// Returns serialized ops for broadcasting to subscribers.
    pub fn edit_text(&mut self, pos: usize, insert: &str, delete: usize) -> Result<Vec<u8>, String> {
        let frontier_before: Frontier = self.doc.version().clone();

        // Compute text length before borrowing doc mutably via writer
        let text_len = self.doc.text_content(&[INPUT_TEXT_KEY])
            .map(|s| s.len())
            .unwrap_or(0);

        {
            let mut w = self.doc.writer(self.agent);
            if delete > 0 {
                let end = (pos + delete).min(text_len);
                if pos < end {
                    w.text_delete(&[INPUT_TEXT_KEY], pos..end);
                }
            }
            if !insert.is_empty() {
                w.text_insert(&[INPUT_TEXT_KEY], pos, insert);
            }
        }

        self.version.fetch_add(1, Ordering::SeqCst);

        let ops = self.doc.ops_since_owned(&frontier_before);
        let ops_bytes = postcard::to_allocvec(&ops)
            .map_err(|e| format!("Failed to serialize input ops: {}", e))?;
        Ok(ops_bytes)
    }

    /// Get the current text content.
    pub fn get_text(&self) -> String {
        self.doc.text_content(&[INPUT_TEXT_KEY]).unwrap_or_default()
    }

    /// Get serialized ops since a frontier (for sync).
    pub fn ops_since(&self, frontier: &Frontier) -> Result<Vec<u8>, String> {
        let ops = self.doc.ops_since_owned(frontier);
        postcard::to_allocvec(&ops)
            .map_err(|e| format!("Failed to serialize ops: {}", e))
    }

    /// Get all ops from the beginning (for full state transfer).
    pub fn all_ops(&self) -> Result<Vec<u8>, String> {
        self.ops_since(&Frontier::root())
    }

    /// Merge remote ops into this document.
    pub fn merge_ops(&mut self, ops_bytes: &[u8]) -> Result<(), String> {
        let ops: SerializedOpsOwned = postcard::from_bytes(ops_bytes)
            .map_err(|e| format!("Failed to deserialize ops: {}", e))?;
        self.doc.merge_ops(ops)
            .map_err(|e| format!("Failed to merge ops: {}", e))?;
        self.version.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    /// Clear the input document, returning the text that was in it.
    ///
    /// Deletes all content and returns what was there + the clear ops.
    /// Used during submit to snapshot the text before clearing.
    pub fn clear(&mut self) -> Result<(String, Vec<u8>), String> {
        let text = self.get_text();
        if text.is_empty() {
            return Ok((String::new(), Vec::new()));
        }

        let frontier_before: Frontier = self.doc.version().clone();

        {
            let len = text.len();
            let mut w = self.doc.writer(self.agent);
            w.text_delete(&[INPUT_TEXT_KEY], 0..len);
        }

        self.version.fetch_add(1, Ordering::SeqCst);

        let ops = self.doc.ops_since_owned(&frontier_before);
        let ops_bytes = postcard::to_allocvec(&ops)
            .map_err(|e| format!("Failed to serialize clear ops: {}", e))?;

        Ok((text, ops_bytes))
    }

    /// Get the current version counter.
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::SeqCst)
    }

    /// Get the current DTE frontier for sync.
    pub fn frontier(&self) -> Frontier {
        self.doc.version().clone()
    }


    /// Record that a principal modified the input.
    pub fn touch(&self, principal_id: PrincipalId) {
        *self.last_agent.write() = principal_id;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_principal() -> PrincipalId {
        PrincipalId::new()
    }

    #[test]
    fn test_new_input_doc_is_empty() {
        let doc = InputDocEntry::new(test_principal());
        assert_eq!(doc.get_text(), "");
        assert_eq!(doc.version(), 0);
    }

    #[test]
    fn test_edit_insert() {
        let mut doc = InputDocEntry::new(test_principal());

        let ops = doc.edit_text(0, "hello", 0).unwrap();
        assert!(!ops.is_empty());
        assert_eq!(doc.get_text(), "hello");
        assert_eq!(doc.version(), 1);
    }

    #[test]
    fn test_edit_insert_and_delete() {
        let mut doc = InputDocEntry::new(test_principal());

        doc.edit_text(0, "hello world", 0).unwrap();
        assert_eq!(doc.get_text(), "hello world");

        // Delete "world" and insert "rust"
        doc.edit_text(6, "rust", 5).unwrap();
        assert_eq!(doc.get_text(), "hello rust");
    }

    #[test]
    fn test_clear() {
        let mut doc = InputDocEntry::new(test_principal());

        doc.edit_text(0, "some draft text", 0).unwrap();
        assert_eq!(doc.get_text(), "some draft text");

        let (text, ops) = doc.clear().unwrap();
        assert_eq!(text, "some draft text");
        assert!(!ops.is_empty());
        assert_eq!(doc.get_text(), "");
    }

    #[test]
    fn test_clear_empty() {
        let mut doc = InputDocEntry::new(test_principal());
        let (text, ops) = doc.clear().unwrap();
        assert_eq!(text, "");
        assert!(ops.is_empty());
    }

    #[test]
    fn test_ops_roundtrip_via_from_ops() {
        let principal = test_principal();
        let mut doc1 = InputDocEntry::new(principal);

        doc1.edit_text(0, "hello", 0).unwrap();
        let ops_bytes = doc1.all_ops().unwrap();

        // Reconstruct via from_ops (correct sync pattern — bare doc + merge)
        let doc2 = InputDocEntry::from_ops(&ops_bytes, PrincipalId::new()).unwrap();
        assert_eq!(doc2.get_text(), "hello");
    }

    #[test]
    fn test_incremental_merge_on_same_doc() {
        let mut doc = InputDocEntry::new(test_principal());

        doc.edit_text(0, "hello", 0).unwrap();
        let frontier_after_hello = doc.frontier();

        doc.edit_text(5, " world", 0).unwrap();

        // Get only the " world" ops
        let incremental_ops = doc.ops_since(&frontier_after_hello).unwrap();

        // Reconstruct full state, then apply incremental
        let hello_ops = doc.ops_since(&Frontier::root()).unwrap();
        let mut doc2 = InputDocEntry::from_ops(&hello_ops, PrincipalId::new()).unwrap();
        assert!(doc2.get_text().contains("hello"));

        // Apply incremental ops
        doc2.merge_ops(&incremental_ops).unwrap();
        assert_eq!(doc2.get_text(), "hello world");
    }

    #[test]
    fn test_from_ops_restore() {
        let principal = test_principal();
        let mut doc = InputDocEntry::new(principal);

        doc.edit_text(0, "restored content", 0).unwrap();
        let ops_bytes = doc.all_ops().unwrap();

        // Restore from ops
        let restored = InputDocEntry::from_ops(&ops_bytes, PrincipalId::new()).unwrap();
        assert_eq!(restored.get_text(), "restored content");
    }
}
