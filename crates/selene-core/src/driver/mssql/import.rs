//! `CREATE TABLE` and `INSERT` for CSV import.
//!
//! Inserts use **multi-row parameterised** statements — values are always bound
//! (`@P1`, `@P2`, …), never spliced into SQL text, so cell data can never inject.
//! SQL Server caps a single request at 2100 parameters, so each statement is
//! sub-batched to at most `2000 / column_count` rows. An *atomic* import wraps
//! every batch (and any preceding `CREATE TABLE`) in one transaction that rolls
//! back on the first error.

use std::fmt::Write as _;

use tiberius::ToSql;

use crate::driver::{CancelToken, ImportTarget, NewColumn, RowSource};
use crate::error::CoreError;
use crate::value::CellValue;

use super::convert::{value_to_param, SqlParam};
use super::error::map_tiberius_err;
use super::introspect::quote_ident;
use super::stream::TiberiusClient;

/// Stay safely under SQL Server's 2100-parameter-per-request limit.
const MAX_PARAMS: usize = 2000;

/// Build a bracket-quoted, optionally three-part table name.
fn qualify(database: Option<&str>, schema: &str, table: &str) -> String {
    match database {
        Some(db) if !db.is_empty() => format!(
            "{}.{}.{}",
            quote_ident(db),
            quote_ident(schema),
            quote_ident(table)
        ),
        _ => format!("{}.{}", quote_ident(schema), quote_ident(table)),
    }
}

/// Validate a DDL type fragment before splicing it into `CREATE TABLE`. Only
/// characters that can legitimately appear in a SQL Server type are allowed, so
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

/// Create a table from `columns`. Every identifier is bracket-quoted and every
/// type fragment is validated.
pub(super) async fn create_table(
    client: &mut TiberiusClient,
    database: Option<&str>,
    schema: &str,
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
        qualify(database, schema, table),
        defs.join(", ")
    );
    run_batch(client, &ddl).await
}

/// Drop `table`. Every identifier is bracket-quoted. Plain `DROP TABLE` (not
/// `IF EXISTS`): the import-retry caller only reaches here after a confirmed
/// "table already exists" failure, so a missing table is itself worth
/// surfacing rather than silently swallowing.
pub(super) async fn drop_table(
    client: &mut TiberiusClient,
    database: Option<&str>,
    schema: &str,
    table: &str,
    cancel: &CancelToken,
) -> Result<(), CoreError> {
    if cancel.is_cancelled() {
        return Err(CoreError::Cancelled);
    }
    let ddl = format!("DROP TABLE {}", qualify(database, schema, table));
    run_batch(client, &ddl).await
}

/// Run a parameterless statement and drain its (empty) result stream.
async fn run_batch(client: &mut TiberiusClient, sql: &str) -> Result<(), CoreError> {
    client
        .simple_query(sql)
        .await
        .map_err(map_tiberius_err)?
        .into_results()
        .await
        .map_err(map_tiberius_err)?;
    Ok(())
}

/// Insert all rows pulled from `source` into `target`. See the module docs for
/// the batching and transaction model.
pub(super) async fn import_rows(
    client: &mut TiberiusClient,
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

    let qualified = qualify(target.database.as_deref(), &target.schema, &target.table);
    let col_list = target
        .columns
        .iter()
        .map(|c| quote_ident(c))
        .collect::<Vec<_>>()
        .join(", ");
    let rows_per_stmt = (MAX_PARAMS / col_count).max(1).min(batch_size.max(1));

    if atomic {
        run_batch(client, "BEGIN TRANSACTION").await?;
    }

    let mut inserted: u64 = 0;
    let outcome = insert_loop(
        client,
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
            if atomic {
                run_batch(client, "COMMIT").await?;
            }
            Ok(inserted)
        }
        Err(e) => {
            if atomic {
                // Best-effort rollback; surface the original error regardless.
                let _ = run_batch(client, "IF @@TRANCOUNT > 0 ROLLBACK").await;
            }
            Err(e)
        }
    }
}

/// Pull batches from `source` and flush them in parameter-capped chunks.
#[allow(clippy::too_many_arguments)]
async fn insert_loop(
    client: &mut TiberiusClient,
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
            *inserted += insert_chunk(client, qualified, col_list, col_count, chunk).await?;
        }
    }
    Ok(())
}

/// Build and run one multi-row parameterised `INSERT`, returning rows affected.
async fn insert_chunk(
    client: &mut TiberiusClient,
    qualified: &str,
    col_list: &str,
    col_count: usize,
    rows: &[Vec<CellValue>],
) -> Result<u64, CoreError> {
    let mut sql = format!("INSERT INTO {qualified} ({col_list}) VALUES ");
    let mut params: Vec<SqlParam> = Vec::with_capacity(rows.len() * col_count);
    let mut p = 1usize;

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
        for (ci, cell) in row.iter().enumerate() {
            if ci > 0 {
                sql.push(',');
            }
            // `write!` to a String is infallible.
            let _ = write!(sql, "@P{p}");
            p += 1;
            params.push(value_to_param(cell));
        }
        sql.push(')');
    }

    let bound: Vec<&dyn ToSql> = params.iter().map(|x| x as &dyn ToSql).collect();
    let result = client
        .execute(sql.as_str(), &bound)
        .await
        .map_err(map_tiberius_err)?;
    Ok(result.total())
}
