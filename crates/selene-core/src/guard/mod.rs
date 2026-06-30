//! The SQL safety guard.
//!
//! A heuristic classifier ([`sql_guard`]) that inspects a SQL batch before it
//! runs and returns an advisory [`GuardVerdict`] — benign, confirm, or block —
//! driving the editor's "are you sure?" prompts and the read-only safety
//! toggle. Pure logic, no dependencies, fully unit-tested.

pub mod sql_guard;

pub use sql_guard::{classify, GuardLevel, GuardVerdict};

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
