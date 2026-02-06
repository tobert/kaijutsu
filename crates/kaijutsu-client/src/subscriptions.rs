//! Server-push event types and Cap'n Proto callback forwarders.
//!
//! Provides [`ServerEvent`] — a typed enum of all events the server pushes
//! to the client — and [`ConnectionStatus`] for connection lifecycle tracking.
//!
//! The callback forwarder structs (`BlockEventsForwarder`, `ResourceEventsForwarder`)
//! implement Cap'n Proto's generated Server traits and bridge incoming RPC callbacks
//! into tokio broadcast channels that [`ActorHandle`](crate::ActorHandle) consumers
//! can subscribe to.

use std::rc::Rc;

use capnp::capability::Promise;
use kaijutsu_crdt::{BlockId, BlockSnapshot};
use tokio::sync::broadcast;

use crate::kaijutsu_capnp::{block_events, resource_events};
use crate::rpc::{parse_block_id, parse_block_snapshot};

// ============================================================================
// Event Types
// ============================================================================

/// Events pushed from server to app via broadcast.
///
/// These are the typed, deserialized forms of Cap'n Proto callback invocations.
/// Subscribe via [`ActorHandle::subscribe_events()`](crate::ActorHandle::subscribe_events).
#[derive(Clone, Debug)]
pub enum ServerEvent {
    /// A new block was inserted into a document.
    BlockInserted {
        document_id: String,
        block: Box<BlockSnapshot>,
        ops: Vec<u8>,
    },
    /// CRDT text operations applied to a block's content.
    BlockTextOps {
        document_id: String,
        block_id: BlockId,
        ops: Vec<u8>,
    },
    /// A block's execution status changed (Pending → Running → Done/Error).
    BlockStatusChanged {
        document_id: String,
        block_id: BlockId,
        status: kaijutsu_crdt::Status,
    },
    /// A block was deleted from a document.
    BlockDeleted {
        document_id: String,
        block_id: BlockId,
    },
    /// A block's collapsed state changed.
    BlockCollapsedChanged {
        document_id: String,
        block_id: BlockId,
        collapsed: bool,
    },
    /// A block was moved to a new position in the document.
    BlockMoved {
        document_id: String,
        block_id: BlockId,
        after_id: Option<BlockId>,
    },
    /// An MCP resource's content was updated.
    ResourceUpdated {
        server: String,
        uri: String,
    },
    /// An MCP server's resource list changed.
    ResourceListChanged {
        server: String,
    },
}

/// Connection lifecycle status.
///
/// Subscribe via [`ActorHandle::subscribe_status()`](crate::ActorHandle::subscribe_status).
#[derive(Clone, Debug)]
pub enum ConnectionStatus {
    Connected,
    Disconnected,
    Reconnecting { attempt: u32 },
    Error(String),
}

/// Monotonic generation counter — bumped on lag or reconnect.
///
/// Consumers can compare generations to detect stale data.
#[derive(Clone, Debug, Default)]
pub struct SyncGeneration(pub u64);

// ============================================================================
// Block Events Forwarder
// ============================================================================

/// Implements the Cap'n Proto `BlockEvents::Server` trait, forwarding each
/// callback into a `broadcast::Sender<ServerEvent>`.
pub(crate) struct BlockEventsForwarder {
    pub event_tx: broadcast::Sender<ServerEvent>,
}

/// Extract a String from a capnp text reader, mapping errors to capnp::Error.
fn read_text(reader: capnp::text::Reader<'_>) -> Result<String, capnp::Error> {
    reader.to_str().map(|s| s.to_owned()).map_err(|e| capnp::Error::failed(e.to_string()))
}

/// Convert an RpcError from our parsing helpers into a capnp::Error.
fn rpc_to_capnp(e: crate::rpc::RpcError) -> capnp::Error {
    capnp::Error::failed(e.to_string())
}

/// Parse a Status enum from capnp.
fn parse_status(status: crate::kaijutsu_capnp::Status) -> kaijutsu_crdt::Status {
    match status {
        crate::kaijutsu_capnp::Status::Pending => kaijutsu_crdt::Status::Pending,
        crate::kaijutsu_capnp::Status::Running => kaijutsu_crdt::Status::Running,
        crate::kaijutsu_capnp::Status::Done => kaijutsu_crdt::Status::Done,
        crate::kaijutsu_capnp::Status::Error => kaijutsu_crdt::Status::Error,
    }
}

#[allow(refining_impl_trait)]
impl block_events::Server for BlockEventsForwarder {
    fn on_block_inserted(
        self: Rc<Self>,
        params: block_events::OnBlockInsertedParams,
        _results: block_events::OnBlockInsertedResults,
    ) -> Promise<(), capnp::Error> {
        let params = match params.get() {
            Ok(p) => p,
            Err(e) => return Promise::err(e),
        };

        let document_id = match params.get_document_id() {
            Ok(s) => match read_text(s) {
                Ok(s) => s,
                Err(e) => return Promise::err(e),
            },
            Err(e) => return Promise::err(e),
        };

        let block = match params.get_block() {
            Ok(b) => match parse_block_snapshot(&b) {
                Ok(snap) => Box::new(snap),
                Err(e) => return Promise::err(rpc_to_capnp(e)),
            },
            Err(e) => return Promise::err(e),
        };

        let ops = params.get_ops().map(|d| d.to_vec()).unwrap_or_default();

        let _ = self.event_tx.send(ServerEvent::BlockInserted { document_id, block, ops });
        Promise::ok(())
    }

    fn on_block_deleted(
        self: Rc<Self>,
        params: block_events::OnBlockDeletedParams,
        _results: block_events::OnBlockDeletedResults,
    ) -> Promise<(), capnp::Error> {
        let params = match params.get() {
            Ok(p) => p,
            Err(e) => return Promise::err(e),
        };

        let document_id = match params.get_document_id() {
            Ok(s) => match read_text(s) {
                Ok(s) => s,
                Err(e) => return Promise::err(e),
            },
            Err(e) => return Promise::err(e),
        };

        let block_id = match params.get_block_id() {
            Ok(b) => match parse_block_id(&b) {
                Ok(id) => id,
                Err(e) => return Promise::err(rpc_to_capnp(e)),
            },
            Err(e) => return Promise::err(e),
        };

        let _ = self.event_tx.send(ServerEvent::BlockDeleted { document_id, block_id });
        Promise::ok(())
    }

    fn on_block_collapsed(
        self: Rc<Self>,
        params: block_events::OnBlockCollapsedParams,
        _results: block_events::OnBlockCollapsedResults,
    ) -> Promise<(), capnp::Error> {
        let params = match params.get() {
            Ok(p) => p,
            Err(e) => return Promise::err(e),
        };

        let document_id = match params.get_document_id() {
            Ok(s) => match read_text(s) {
                Ok(s) => s,
                Err(e) => return Promise::err(e),
            },
            Err(e) => return Promise::err(e),
        };

        let block_id = match params.get_block_id() {
            Ok(b) => match parse_block_id(&b) {
                Ok(id) => id,
                Err(e) => return Promise::err(rpc_to_capnp(e)),
            },
            Err(e) => return Promise::err(e),
        };

        let _ = self.event_tx.send(ServerEvent::BlockCollapsedChanged {
            document_id,
            block_id,
            collapsed: params.get_collapsed(),
        });
        Promise::ok(())
    }

    fn on_block_moved(
        self: Rc<Self>,
        params: block_events::OnBlockMovedParams,
        _results: block_events::OnBlockMovedResults,
    ) -> Promise<(), capnp::Error> {
        let params = match params.get() {
            Ok(p) => p,
            Err(e) => return Promise::err(e),
        };

        let document_id = match params.get_document_id() {
            Ok(s) => match read_text(s) {
                Ok(s) => s,
                Err(e) => return Promise::err(e),
            },
            Err(e) => return Promise::err(e),
        };

        let block_id = match params.get_block_id() {
            Ok(b) => match parse_block_id(&b) {
                Ok(id) => id,
                Err(e) => return Promise::err(rpc_to_capnp(e)),
            },
            Err(e) => return Promise::err(e),
        };

        let after_id = if params.get_has_after_id() {
            match params.get_after_id() {
                Ok(b) => match parse_block_id(&b) {
                    Ok(id) => Some(id),
                    Err(e) => return Promise::err(rpc_to_capnp(e)),
                },
                Err(e) => return Promise::err(e),
            }
        } else {
            None
        };

        let _ = self.event_tx.send(ServerEvent::BlockMoved { document_id, block_id, after_id });
        Promise::ok(())
    }

    fn on_block_status_changed(
        self: Rc<Self>,
        params: block_events::OnBlockStatusChangedParams,
        _results: block_events::OnBlockStatusChangedResults,
    ) -> Promise<(), capnp::Error> {
        let params = match params.get() {
            Ok(p) => p,
            Err(e) => return Promise::err(e),
        };

        let document_id = match params.get_document_id() {
            Ok(s) => match read_text(s) {
                Ok(s) => s,
                Err(e) => return Promise::err(e),
            },
            Err(e) => return Promise::err(e),
        };

        let block_id = match params.get_block_id() {
            Ok(b) => match parse_block_id(&b) {
                Ok(id) => id,
                Err(e) => return Promise::err(rpc_to_capnp(e)),
            },
            Err(e) => return Promise::err(e),
        };

        let status = match params.get_status() {
            Ok(s) => parse_status(s),
            Err(e) => return Promise::err(e.into()),
        };

        let _ = self.event_tx.send(ServerEvent::BlockStatusChanged {
            document_id,
            block_id,
            status,
        });
        Promise::ok(())
    }

    fn on_block_text_ops(
        self: Rc<Self>,
        params: block_events::OnBlockTextOpsParams,
        _results: block_events::OnBlockTextOpsResults,
    ) -> Promise<(), capnp::Error> {
        let params = match params.get() {
            Ok(p) => p,
            Err(e) => return Promise::err(e),
        };

        let document_id = match params.get_document_id() {
            Ok(s) => match read_text(s) {
                Ok(s) => s,
                Err(e) => return Promise::err(e),
            },
            Err(e) => return Promise::err(e),
        };

        let block_id = match params.get_block_id() {
            Ok(b) => match parse_block_id(&b) {
                Ok(id) => id,
                Err(e) => return Promise::err(rpc_to_capnp(e)),
            },
            Err(e) => return Promise::err(e),
        };

        let ops = match params.get_ops() {
            Ok(data) => data.to_vec(),
            Err(e) => return Promise::err(e),
        };

        let _ = self.event_tx.send(ServerEvent::BlockTextOps { document_id, block_id, ops });
        Promise::ok(())
    }
}

// ============================================================================
// Resource Events Forwarder
// ============================================================================

/// Implements the Cap'n Proto `ResourceEvents::Server` trait, forwarding
/// resource update notifications into a `broadcast::Sender<ServerEvent>`.
pub(crate) struct ResourceEventsForwarder {
    pub event_tx: broadcast::Sender<ServerEvent>,
}

#[allow(refining_impl_trait)]
impl resource_events::Server for ResourceEventsForwarder {
    fn on_resource_updated(
        self: Rc<Self>,
        params: resource_events::OnResourceUpdatedParams,
        _results: resource_events::OnResourceUpdatedResults,
    ) -> Promise<(), capnp::Error> {
        let params = match params.get() {
            Ok(p) => p,
            Err(e) => return Promise::err(e),
        };

        let server = match params.get_server() {
            Ok(s) => match read_text(s) {
                Ok(s) => s,
                Err(e) => return Promise::err(e),
            },
            Err(e) => return Promise::err(e),
        };

        let uri = match params.get_uri() {
            Ok(s) => match read_text(s) {
                Ok(s) => s,
                Err(e) => return Promise::err(e),
            },
            Err(e) => return Promise::err(e),
        };

        let _ = self.event_tx.send(ServerEvent::ResourceUpdated { server, uri });
        Promise::ok(())
    }

    fn on_resource_list_changed(
        self: Rc<Self>,
        params: resource_events::OnResourceListChangedParams,
        _results: resource_events::OnResourceListChangedResults,
    ) -> Promise<(), capnp::Error> {
        let params = match params.get() {
            Ok(p) => p,
            Err(e) => return Promise::err(e),
        };

        let server = match params.get_server() {
            Ok(s) => match read_text(s) {
                Ok(s) => s,
                Err(e) => return Promise::err(e),
            },
            Err(e) => return Promise::err(e),
        };

        let _ = self.event_tx.send(ServerEvent::ResourceListChanged { server });
        Promise::ok(())
    }
}
