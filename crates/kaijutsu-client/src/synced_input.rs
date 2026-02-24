//! Client-side synced input document (compose scratchpad).
//!
//! [`SyncedInput`] wraps a single diamond-types-extended `Document` with a
//! root-level Text CRDT keyed by `"input"`. Much simpler than [`SyncedDocument`]
//! since there are no blocks — just collaborative text.

use diamond_types_extended::{AgentId, Document, SerializedOpsOwned, Uuid};
use kaijutsu_types::{ContextId, PrincipalId};

/// Client-side synced input document (compose scratchpad).
///
/// Each context has an input document that holds the current compose text.
/// This wrapper provides a simple edit/sync API over the DTE Document.
pub struct SyncedInput {
    doc: Document,
    agent: AgentId,
    context_id: ContextId,
}

const INPUT_TEXT_KEY: &str = "input";

impl SyncedInput {
    /// Create a new empty input doc for a context.
    ///
    /// Pre-registers `PrincipalId::system()` as a known DTE agent so that
    /// server-originated ops (which use the system principal) can merge
    /// without `ParseError::DataMissing`.
    pub fn new(context_id: ContextId, principal_id: PrincipalId) -> Self {
        let mut doc = Document::new();
        let agent = doc.create_agent(Uuid::from_bytes(*principal_id.as_bytes()));
        // Pre-register the server's system agent so remote ops can merge
        let _system = doc.create_agent(Uuid::from_bytes(*PrincipalId::system().as_bytes()));
        {
            let mut w = doc.writer(agent);
            w.root_create_text(INPUT_TEXT_KEY);
        }
        Self {
            doc,
            agent,
            context_id,
        }
    }

    /// Create from server state (ops from `get_input_state` RPC).
    pub fn from_state(
        context_id: ContextId,
        principal_id: PrincipalId,
        ops: &[u8],
    ) -> Result<Self, String> {
        let remote_ops: SerializedOpsOwned =
            postcard::from_bytes(ops).map_err(|e| format!("deserialize input ops: {}", e))?;
        let mut doc = Document::new();
        let agent = doc.create_agent(Uuid::from_bytes(*principal_id.as_bytes()));
        doc.merge_ops(remote_ops)
            .map_err(|e| format!("merge input ops: {}", e))?;
        Ok(Self {
            doc,
            agent,
            context_id,
        })
    }

    /// Apply a local edit and return ops to send to server.
    pub fn edit(&mut self, pos: usize, insert: &str, delete: usize) -> Vec<u8> {
        let frontier_before = self.doc.version().clone();
        let text_len = self
            .doc
            .text_content(&[INPUT_TEXT_KEY])
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
        let ops = self.doc.ops_since_owned(&frontier_before);
        postcard::to_allocvec(&ops).unwrap_or_default()
    }

    /// Get current text content.
    pub fn text(&self) -> String {
        self.doc
            .text_content(&[INPUT_TEXT_KEY])
            .unwrap_or_default()
    }

    /// Apply remote ops (from `InputTextOps` server event).
    pub fn apply_remote_ops(&mut self, ops: &[u8]) -> Result<(), String> {
        let remote_ops: SerializedOpsOwned =
            postcard::from_bytes(ops).map_err(|e| format!("deserialize remote ops: {}", e))?;
        self.doc
            .merge_ops(remote_ops)
            .map_err(|e| format!("merge remote ops: {}", e))?;
        Ok(())
    }

    /// Clear the input (after `InputCleared` server event).
    pub fn clear(&mut self) {
        let text_len = self
            .doc
            .text_content(&[INPUT_TEXT_KEY])
            .map(|s| s.len())
            .unwrap_or(0);
        if text_len > 0 {
            let mut w = self.doc.writer(self.agent);
            w.text_delete(&[INPUT_TEXT_KEY], 0..text_len);
        }
    }

    /// The context ID this input document belongs to.
    pub fn context_id(&self) -> ContextId {
        self.context_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_context_id() -> ContextId {
        ContextId::new()
    }

    fn test_principal_id() -> PrincipalId {
        PrincipalId::new()
    }

    #[test]
    fn test_new_empty() {
        let input = SyncedInput::new(test_context_id(), test_principal_id());
        assert_eq!(input.text(), "");
    }

    #[test]
    fn test_edit_insert() {
        let mut input = SyncedInput::new(test_context_id(), test_principal_id());
        let _ops = input.edit(0, "hello", 0);
        assert_eq!(input.text(), "hello");
    }

    #[test]
    fn test_edit_delete() {
        let mut input = SyncedInput::new(test_context_id(), test_principal_id());
        input.edit(0, "hello world", 0);
        let _ops = input.edit(5, "", 6); // delete " world"
        assert_eq!(input.text(), "hello");
    }

    #[test]
    fn test_edit_replace() {
        let mut input = SyncedInput::new(test_context_id(), test_principal_id());
        input.edit(0, "hello", 0);
        let _ops = input.edit(0, "goodbye", 5);
        assert_eq!(input.text(), "goodbye");
    }

    #[test]
    fn test_clear() {
        let mut input = SyncedInput::new(test_context_id(), test_principal_id());
        input.edit(0, "some text", 0);
        input.clear();
        assert_eq!(input.text(), "");
    }

    #[test]
    fn test_ops_roundtrip() {
        let ctx = test_context_id();
        let mut input_a = SyncedInput::new(ctx, test_principal_id());

        // First, get the full state so input_b starts from the same base
        let full_ops = {
            let frontier = diamond_types_extended::Frontier::root();
            let ops = input_a.doc.ops_since_owned(&frontier);
            postcard::to_allocvec(&ops).unwrap()
        };
        let mut input_b = SyncedInput::from_state(ctx, test_principal_id(), &full_ops).unwrap();

        // Now an incremental edit on A can be applied to B
        let ops = input_a.edit(0, "synced!", 0);
        input_b.apply_remote_ops(&ops).unwrap();
        assert_eq!(input_b.text(), "synced!");
    }

    #[test]
    fn test_from_state_roundtrip() {
        let ctx = test_context_id();
        let pid = test_principal_id();
        let mut input = SyncedInput::new(ctx, pid);
        input.edit(0, "persisted", 0);

        // Serialize full state (ops since root)
        let full_ops = {
            let frontier = diamond_types_extended::Frontier::root();
            let ops = input.doc.ops_since_owned(&frontier);
            postcard::to_allocvec(&ops).unwrap()
        };

        // Reconstruct from state
        let restored = SyncedInput::from_state(ctx, test_principal_id(), &full_ops).unwrap();
        assert_eq!(restored.text(), "persisted");
        assert_eq!(restored.context_id(), ctx);
    }

    #[test]
    fn test_context_id_accessor() {
        let ctx = test_context_id();
        let input = SyncedInput::new(ctx, test_principal_id());
        assert_eq!(input.context_id(), ctx);
    }

    #[test]
    fn test_system_agent_ops_via_from_state() {
        // Verify that system agent ops work when client is created via from_state
        // (the normal path: server creates doc, client gets full state, then incremental ops)
        let ctx = test_context_id();
        let system_pid = PrincipalId::system();

        // Create a "server-side" doc using the system agent
        let mut server_doc = Document::new();
        let server_agent = server_doc.create_agent(Uuid::from_bytes(*system_pid.as_bytes()));
        server_doc.transact(server_agent, |tx| {
            tx.root().create_text(INPUT_TEXT_KEY);
        });

        // Client gets full state from server (the normal sync path)
        let full_ops = {
            let ops = server_doc.ops_since_owned(&diamond_types_extended::Frontier::root());
            postcard::to_allocvec(&ops).unwrap()
        };
        let mut client = SyncedInput::from_state(ctx, test_principal_id(), &full_ops).unwrap();

        // Now server edits with system agent — incremental ops should merge
        let frontier_before = server_doc.version().clone();
        server_doc.transact(server_agent, |tx| {
            if let Some(mut text) = tx.get_text_mut(&[INPUT_TEXT_KEY]) {
                text.insert(0, "server edit");
            }
        });
        let incremental_ops = server_doc.ops_since_owned(&frontier_before);
        let ops_bytes = postcard::to_allocvec(&incremental_ops).unwrap();

        // Client should accept these ops because it has the full causal history
        client.apply_remote_ops(&ops_bytes).unwrap();
        assert_eq!(client.text(), "server edit");
    }

    #[test]
    fn test_system_agent_preregistered_in_new() {
        // Verify that PrincipalId::system() is registered as a known agent
        // in SyncedInput::new(). This is a best-effort measure — it helps when
        // the only issue is unknown agent UUID, but doesn't solve divergent
        // document structure (which requires from_state).
        let ctx = test_context_id();
        let input = SyncedInput::new(ctx, test_principal_id());
        // The doc should have 2 agents: client + system
        // (No direct way to assert this, but verify it doesn't panic)
        assert_eq!(input.text(), "");
    }
}
