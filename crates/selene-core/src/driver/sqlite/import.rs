//! `CREATE TABLE` / `DROP TABLE` / multi-row `INSERT` for CSV import into SQLite.
//!
//! Inserts use **bound parameters** (`?` placeholders) — cell data is never
//! spliced into SQL text, so it can never inject. SQLite caps a statement at 999
//! bound parameters (`SQLITE_MAX_VARIABLE_NUMBER`), so each `INSERT` is
//! sub-batched to at most `999 / column_count` rows (see
//! [`rows_per_statement`](crate::driver::shared::sub_batch::rows_per_statement)).
//! An *atomic* import wraps every batch (and any preceding `CREATE TABLE`,
//! handled by the caller) in one transaction that rolls back on the first error.

use std::fmt::Write as _;

use sqlx::sqlite::SqliteConnection;
use sqlx::{Connection as _, Executor as _};

use crate::driver::shared::sub_batch::rows_per_statement;
use crate::driver::{CancelToken, ImportTarget, NewColumn, RowSource};
use crate::error::CoreError;
use crate::value::CellValue;

use super::convert::bind_value;
use super::error::map_sqlx_err;
use super::introspect::quote_ident;

/// SQLite's default bound-parameter cap (`SQLITE_MAX_VARIABLE_NUMBER`).
const MAX_PARAMS: usize = 999;

/// Build a double-quoted table name. SQLite has no schema level, so `schema` and
/// `database` are ignored (a CSV import always targets the `main` database's
/// table namespace).
fn qualify(table: &str) -> String {
    quote_ident(table)
}

/// Validate a DDL type fragment before splicing it into `CREATE TABLE`. Only
/// characters that can legitimately appear in a SQLite column type are allowed,
/// so the fragment cannot break out of the type position. (SQLite is permissive
/// about type names, but we still gate the splice.)
fn validate_type(sql_type: &str) -> Result<(), CoreError> {
    let ok = !sql_type.is_empty()
        && sql_type.len() <= 64
        && sql_type
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '(' | ')' | ',' | ' '));
    if ok {
        Ok(())
    } else {
        Err(CoreError::Import(format!(
            "invalid column type {sql_type:?}"
        )))
    }
}

/// Create a table from `columns`. Every identifier is double-quoted and every
/// type fragment is validated.
pub(super) async fn create_table(
    conn: &mut SqliteConnection,
    table: &str,
    columns: &[NewColumn],
    cancel: &CancelToken,
) -> Result<(), CoreError> {
    if cancel.is_cancelled() {
        return Err(CoreError::Cancelled);
    }
    if columns.is_empty() {
        return Err(CoreError::Import(
            "cannot create a table with no columns".into(),
        ));
    }
    let mut defs = Vec::with_capacity(columns.len());
    for c in columns {
        validate_type(&c.sql_type)?;
        let null = if c.nullable { "NULL" } else { "NOT NULL" };
        defs.push(format!("{} {} {}", quote_ident(&c.name), c.sql_type, null));
    }
    let ddl = format!("CREATE TABLE {} ({})", qualify(table), defs.join(", "));
    run_batch(conn, &ddl).await
}

/// Drop `table`. Plain `DROP TABLE` (not `IF EXISTS`): the import-retry caller
/// only reaches here after a confirmed "table already exists" failure, so a
/// missing table is worth surfacing rather than silently swallowing.
pub(super) async fn drop_table(
    conn: &mut SqliteConnection,
    table: &str,
    cancel: &CancelToken,
) -> Result<(), CoreError> {
    if cancel.is_cancelled() {
        return Err(CoreError::Cancelled);
    }
    let ddl = format!("DROP TABLE {}", qualify(table));
    run_batch(conn, &ddl).await
}

/// Run a parameterless statement for its side effects.
async fn run_batch(conn: &mut SqliteConnection, sql: &str) -> Result<(), CoreError> {
    conn.execute(sqlx::raw_sql(sql))
        .await
        .map_err(map_sqlx_err)?;
    Ok(())
}

/// Insert all rows pulled from `source` into `target`. See the module docs for
/// the batching and transaction model.
pub(super) async fn import_rows(
    conn: &mut SqliteConnection,
    target: &ImportTarget,
    source: &mut dyn RowSource,
    atomic: bool,
    batch_size: usize,
    cancel: &CancelToken,
) -> Result<u64, CoreError> {
    let col_count = target.columns.len();
    if col_count == 0 {
        return Err(CoreError::Import("no destination columns".into()));
    }

    let qualified = qualify(&target.table);
    let col_list = target
        .columns
        .iter()
        .map(|c| quote_ident(c))
        .collect::<Vec<_>>()
        .join(", ");
    let rows_per_stmt = rows_per_statement(MAX_PARAMS, col_count, batch_size);

    let mut inserted: u64 = 0;
    if atomic {
        // A real SQLite transaction: `begin()` returns a guard we explicitly
        // commit on success or roll back (best-effort) on the first error. A
        // `Transaction` derefs to `SqliteConnection`, so the insert loop runs
        // against `&mut *tx` and the same code path serves both modes.
        let mut tx = conn.begin().await.map_err(map_sqlx_err)?;
        let outcome = insert_loop(
            &mut tx, // &mut Transaction → &mut SqliteConnection via DerefMut
            source,
            &qualified,
            &col_list,
            col_count,
            rows_per_stmt,
            cancel,
            &mut inserted,
        )
        .await;
        match outcome {
            Ok(()) => {
                tx.commit().await.map_err(map_sqlx_err)?;
                Ok(inserted)
            }
            Err(e) => {
                // Best-effort rollback; surface the original error regardless.
                let _ = tx.rollback().await;
                Err(e)
            }
        }
    } else {
        // Non-atomic: each statement auto-commits as it lands; a skipped (bad)
        // row is dropped by the source itself, so the loop only sees good rows.
        insert_loop(
            conn,
            source,
            &qualified,
            &col_list,
            col_count,
            rows_per_stmt,
            cancel,
            &mut inserted,
        )
        .await?;
        Ok(inserted)
    }
}

/// Pull batches from `source` and flush them in parameter-capped chunks. Takes
/// the connection by `&mut` and reborrows it per statement, so it serves both a
/// live connection and an open transaction (which derefs to a connection).
#[allow(clippy::too_many_arguments)]
async fn insert_loop(
    conn: &mut SqliteConnection,
    source: &mut dyn RowSource,
    qualified: &str,
    col_list: &str,
    col_count: usize,
    rows_per_stmt: usize,
    cancel: &CancelToken,
    inserted: &mut u64,
) -> Result<(), CoreError> {
    loop {
        if cancel.is_cancelled() {
            return Err(CoreError::Cancelled);
        }
        let batch = source.next_batch().await?;
        if batch.is_empty() {
            break;
        }
        for chunk in batch.chunks(rows_per_stmt) {
            if cancel.is_cancelled() {
                return Err(CoreError::Cancelled);
            }
            *inserted += insert_chunk(conn, qualified, col_list, col_count, chunk).await?;
        }
    }
    Ok(())
}

/// Build and run one multi-row parameterised `INSERT`, returning rows affected.
async fn insert_chunk(
    conn: &mut SqliteConnection,
    qualified: &str,
    col_list: &str,
    col_count: usize,
    rows: &[Vec<CellValue>],
) -> Result<u64, CoreError> {
    let mut sql = format!("INSERT INTO {qualified} ({col_list}) VALUES ");
    for (ri, row) in rows.iter().enumerate() {
        if row.len() != col_count {
            return Err(CoreError::Import(format!(
                "row has {} value(s), expected {col_count}",
                row.len()
            )));
        }
        if ri > 0 {
            sql.push(',');
        }
        sql.push('(');
        for ci in 0..col_count {
            if ci > 0 {
                sql.push(',');
            }
            // `write!` to a String is infallible.
            let _ = write!(sql, "?");
        }
        sql.push(')');
    }

    // Bind every cell in row-major order to match the `?` placeholders.
    let mut query = sqlx::query(&sql);
    for row in rows {
        for cell in row {
            query = bind_value(query, cell);
        }
    }

    // `&mut SqliteConnection` is the sqlx `Executor` for a single statement.
    let result = query.execute(&mut *conn).await.map_err(map_sqlx_err)?;
    Ok(result.rows_affected())
}
