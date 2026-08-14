//! The one place this crate reads the wall clock for a value it writes
//! itself (`claimed_at`/`decided_at` — `created_at`/etc. use the SQL
//! `DEFAULT` in `schema.rs` instead, so those never need this).

use std::time::{SystemTime, UNIX_EPOCH};

/// Current time in epoch milliseconds, matching every `DEFAULT` in
/// `schema.rs`. A clock before 1970 would silently corrupt ordering
/// (`idx_approvals_status_created` et al.) if this fell back to 0 instead
/// of failing — per the project's "crashing is preferred over data
/// corruption" stance, it panics instead.
pub(crate) fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before 1970")
        .as_millis() as i64
}
