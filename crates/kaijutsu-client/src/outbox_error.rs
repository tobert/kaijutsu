//! The error a draft or editor outbox returns when it cannot take a send.

/// Why an outbox refused a send. The name says which outbox: `"draft"` or
/// `"editor"`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OutboxError {
    /// The outbox was built with `default()` and no writer is attached.
    #[error("no {0} writer is attached")]
    NotAttached(&'static str),
    /// The writer has ended; nothing will send what was queued.
    #[error("the {0} writer has stopped")]
    Stopped(&'static str),
    /// The writer ended with a flush, submit, or read still waiting on it.
    #[error("the {0} writer stopped before it answered")]
    StoppedBeforeReply(&'static str),
    /// The kernel refused the call; the text is its reason.
    #[error("{0}")]
    Refused(String),
}
