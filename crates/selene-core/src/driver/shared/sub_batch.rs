//! Parameter sub-batching for bound multi-row `INSERT`s.
//!
//! Every SQL engine caps the number of bound parameters per statement (SQLite's
//! default `SQLITE_MAX_VARIABLE_NUMBER` is 999; SQL Server's is 2100). A
//! multi-row `INSERT` binds `rows × columns` parameters, so the import path must
//! split a batch into chunks small enough to stay under the cap. This is the
//! same formula the mssql driver uses, lifted here so every sqlx driver shares
//! it.

/// How many rows fit in one parameterised `INSERT` without exceeding
/// `max_params`, never returning 0 (a single wide row still goes through, even
/// if it nominally exceeds the cap — the engine, not us, then rejects it with a
/// clear error) and never exceeding the caller's desired `batch_size`.
///
/// `(max_params / col_count).max(1).min(batch_size.max(1))`.
pub(crate) fn rows_per_statement(max_params: usize, col_count: usize, batch_size: usize) -> usize {
    // `col_count == 0` is guarded by callers (an INSERT needs columns); guard
    // here too (via `checked_div`) so this stays a total function rather than
    // dividing by zero — a 0 divisor falls back to the caller's batch size.
    let by_params = max_params
        .checked_div(col_count)
        .unwrap_or_else(|| batch_size.max(1));
    by_params.max(1).min(batch_size.max(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SQLite's bound-parameter cap.
    const MAX_PARAMS: usize = 999;

    #[test]
    fn caps_rows_to_stay_under_the_param_limit() {
        // 3 columns: 999 / 3 = 333 rows/statement, below a 500 batch.
        assert_eq!(rows_per_statement(MAX_PARAMS, 3, 500), 333);
        // 10 columns: 999 / 10 = 99 rows/statement.
        assert_eq!(rows_per_statement(MAX_PARAMS, 10, 500), 99);
    }

    #[test]
    fn batch_size_clamps_when_smaller_than_the_param_budget() {
        // 1 column would allow 999 rows, but the caller only wants 500.
        assert_eq!(rows_per_statement(MAX_PARAMS, 1, 500), 500);
    }

    #[test]
    fn never_returns_zero_for_a_very_wide_row() {
        // 2000 columns exceeds the 999-param budget; still allow one row through.
        assert_eq!(rows_per_statement(MAX_PARAMS, 2000, 500), 1);
    }

    #[test]
    fn clamps_to_at_least_one_when_batch_size_is_zero() {
        assert_eq!(rows_per_statement(MAX_PARAMS, 3, 0), 1);
    }

    #[test]
    fn zero_columns_falls_back_to_batch_size() {
        // Defensive: callers guard col_count == 0, but the function stays total.
        assert_eq!(rows_per_statement(MAX_PARAMS, 0, 250), 250);
    }
}
