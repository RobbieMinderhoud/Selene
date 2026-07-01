//! The SQL safety guard.
//!
//! A heuristic classifier ([`sql_guard`]) that inspects a SQL batch before it
//! runs and returns an advisory [`GuardVerdict`] — benign, confirm, or block —
//! driving the editor's "are you sure?" prompts and the read-only safety
//! toggle. Pure logic, no dependencies, fully unit-tested.

pub mod mongo_guard;
pub mod sql_guard;

use crate::connection_spec::DriverId;

pub use mongo_guard::classify_mongo;
pub use sql_guard::{classify, GuardLevel, GuardVerdict};

/// Classify a query for safety, dispatching to the driver-appropriate classifier.
///
/// MongoDB queries are mongosh-shell method calls, not SQL, so they need
/// [`mongo_guard::classify_mongo`]; every SQL backend routes through the
/// keyword-based [`classify`]. This is the entry point the IPC layer uses to
/// enforce the guard server-side (a [`GuardLevel::Block`] refuses the query
/// before it runs), so a write on a read-only MongoDB connection is blocked
/// exactly like a write on a read-only SQL connection.
///
/// Note: this dispatcher (and the `DriverId::Mongodb` arm) is **not** feature
/// gated — [`classify_mongo`] is pure string logic with no `mongodb`/`bson`
/// dependency — so it compiles in every build regardless of which driver
/// features are enabled.
pub fn classify_for(driver: DriverId, query: &str, read_only: bool) -> GuardVerdict {
    match driver {
        DriverId::Mongodb => mongo_guard::classify_mongo(query, read_only),
        _ => classify(query, read_only),
    }
}

// Internal: lets the MSSQL driver route pure-DML batches through the
// affected-count execution path, recognise the rollback-wrapped variant so it
// can trim the wrapper's phantom zero-count entries, and peel leading `USE`
// statements onto the persistent batch path. Not part of the public API. Only
// the mssql driver consumes these, so the re-export is gated to that feature to
// avoid an unused-import warning in sqlx-only builds (the functions themselves
// stay covered by this module's own tests).
#[cfg(feature = "mssql")]
pub(crate) use sql_guard::{
    is_countable_dml_batch, is_rollback_wrapped_dml_batch, peel_leading_use,
};
