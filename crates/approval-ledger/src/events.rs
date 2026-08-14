//! Appending to `approval_events` — the one write path every other module
//! (`claim`, `decide`) routes through, so `seq` assignment (monotonic per
//! `request_id`, computed inside the INSERT the same way `hooks.
//! insertion_idx` is in `kernel_db.rs`) has exactly one implementation.

use rusqlite::{Connection, params};

use crate::error::Result;
use crate::types::EventKind;

#[allow(clippy::too_many_arguments)]
pub(crate) fn append(
    conn: &Connection,
    request_id: &str,
    kind: EventKind,
    actor: Option<&[u8]>,
    decided_option: Option<&str>,
    remember_scope: Option<&str>,
    auto_reason: Option<&str>,
    note: Option<&str>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO approval_events (
            request_id, seq, kind, actor, decided_option, remember_scope, auto_reason, note
         ) VALUES (
            ?1, (SELECT COALESCE(MAX(seq), -1) + 1 FROM approval_events WHERE request_id = ?1),
            ?2, ?3, ?4, ?5, ?6, ?7
         )",
        params![request_id, kind.as_str(), actor, decided_option, remember_scope, auto_reason, note],
    )?;
    Ok(())
}
