//! `CREATE TABLE` / `DROP TABLE` / multi-row `INSERT` for CSV import into MySQL.
//!
//! Inserts use **bound parameters** — cell data is never spliced into SQL text,
//! so it can never inject. MySQL uses `?` placeholders and caps a statement at
//! 65535 bound parameters (`max_prepared_stmt_count`/packet limits aside), so
//! each `INSERT` is sub-batched to at most `65535 / column_count` rows (see
//! [`rows_per_statement`](crate::driver::shared::sub_batch::rows_per_statement)).
//! An *atomic* import wraps every batch (and any preceding `CREATE TABLE`, handled
//! by the caller) in one transaction that rolls back on the first error.

use sqlx::mysql::MySqlConnection;
use sqlx::{Connection as _, Executor as _};

use crate::driver::shared::sub_batch::rows_per_statement;
use crate::driver::{CancelToken, ImportTarget, NewColumn, RowSource};
use crate::error::CoreError;
use crate::value::CellValue;

use super::convert::bind_value;
use super::error::map_sqlx_err;
use super::introspect::quote_ident;

/// MySQL's bound-parameter cap per statement (a 16-bit count → 65535).
const MAX_PARAMS: usize = 65535;

/// Build a backtick-quoted, optionally database-qualified table name
/// (`` `db`.`table` `` when a database is given, else `` `table` ``).
fn qualify(database: Option<&str>, table: &str) -> String {
    match database {
        Some(db) if !db.is_empty() => format!("{}.{}", quote_ident(db), quote_ident(table)),
        _ => quote_ident(table),
    }
}

/// Validate a DDL type fragment before splicing it into `CREATE TABLE`. Only
/// characters that can legitimately appear in a MySQL column type are allowed, so
/// the fragment cannot break out of the type position.
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

/// Create a table from `columns`. Every identifier is backtick-quoted and every
/// type fragment is validated.
pub(super) async fn create_table(
    conn: &mut MySqlConnection,
    database: Option<&str>,
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
    let ddl = format!(
        "CREATE TABLE {} ({})",
        qualify(database, table),
        defs.join(", ")
    );
    run_batch(conn, &ddl).await
}

/// Drop `table`. Plain `DROP TABLE` (not `IF EXISTS`): the import-retry caller
/// only reaches here after a confirmed "table already exists" failure, so a
/// missing table is worth surfacing rather than silently swallowing.
pub(super) async fn drop_table(
    conn: &mut MySqlConnection,
    database: Option<&str>,
    table: &str,
    cancel: &CancelToken,
) -> Result<(), CoreError> {
    if cancel.is_cancelled() {
        return Err(CoreError::Cancelled);
    }
    let ddl = format!("DROP TABLE {}", qualify(database, table));
    run_batch(conn, &ddl).await
}

/// Run a parameterless statement for its side effects.
async fn run_batch(conn: &mut MySqlConnection, sql: &str) -> Result<(), CoreError> {
    conn.execute(sqlx::raw_sql(sql))
        .await
        .map_err(map_sqlx_err)?;
    Ok(())
}

/// Insert all rows pulled from `source` into `target`. See the module docs for
/// the batching and transaction model.
pub(super) async fn import_rows(
    conn: &mut MySqlConnection,
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

    let qualified = qualify(target.database.as_deref(), &target.table);
    let col_list = target
        .columns
        .iter()
        .map(|c| quote_ident(c))
        .collect::<Vec<_>>()
        .join(", ");
    let rows_per_stmt = rows_per_statement(MAX_PARAMS, col_count, batch_size);

    let mut inserted: u64 = 0;
    if atomic {
        // A real MySQL transaction: `begin()` returns a guard we explicitly commit
        // on success or roll back (best-effort) on the first error. A
        // `Transaction` derefs to `MySqlConnection`, so the insert loop runs
        // against `&mut *tx` and the same code path serves both modes.
        let mut tx = conn.begin().await.map_err(map_sqlx_err)?;
        let outcome = insert_loop(
            &mut tx, // &mut Transaction → &mut MySqlConnection via DerefMut
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
        // Non-atomic: each statement auto-commits as it lands; a skipped (bad) row
        // is dropped by the source itself, so the loop only sees good rows.
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
    conn: &mut MySqlConnection,
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
/// Placeholders are MySQL's positional `?` (one per bound value).
async fn insert_chunk(
    conn: &mut MySqlConnection,
    qualified: &str,
    col_list: &str,
    col_count: usize,
    rows: &[Vec<CellValue>],
) -> Result<u64, CoreError> {
    // One `(?, ?, …)` tuple per row, joined with commas.
    let placeholders = vec!["?"; col_count].join(",");
    let tuple = format!("({placeholders})");
    let values = vec![tuple.as_str(); rows.len()].join(",");
    let sql = format!("INSERT INTO {qualified} ({col_list}) VALUES {values}");

    // Validate row shape before binding so a malformed row fails clearly rather
    // than misaligning the `?` placeholders.
    for row in rows {
        if row.len() != col_count {
            return Err(CoreError::Import(format!(
                "row has {} value(s), expected {col_count}",
                row.len()
            )));
        }
    }

    // Bind every cell in row-major order to match the `?` placeholders.
    let mut query = sqlx::query(&sql);
    for row in rows {
        for cell in row {
            query = bind_value(query, cell);
        }
    }

    // `&mut MySqlConnection` is the sqlx `Executor` for a single statement.
    let result = query.execute(&mut *conn).await.map_err(map_sqlx_err)?;
    Ok(result.rows_affected())
}
