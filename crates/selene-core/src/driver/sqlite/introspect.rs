//! Schema introspection for SQLite.
//!
//! SQLite has **no schema level** (it goes database → table → column), so
//! [`list_schemas`] returns empty and the UI skips that tree level (the driver's
//! `capabilities.schemas` is `false`). The other three levels read SQLite's own
//! catalog:
//! - databases ← `PRAGMA database_list` (always `main`, plus any `ATTACH`ed DBs);
//! - tables/views ← `sqlite_master`;
//! - columns ← `PRAGMA table_info("<table>")`.
//!
//! ## SQL-injection safety
//! `PRAGMA table_info(...)` cannot take a bound parameter for its argument, so
//! the table name is interpolated — made safe by [`quote_ident`], which
//! double-quotes the identifier and doubles any embedded `"` (the SQLite
//! analogue of the mssql driver's bracket-quoting). The table-list query binds
//! no user input. `ATTACH` database names from `database_list` are read, not
//! interpolated.

use sqlx::sqlite::SqliteConnection;
use sqlx::Row as _;

use crate::error::CoreError;
use crate::introspect::{ColumnInfo, DatabaseInfo, SchemaInfo, TableInfo, TableKind};

use super::error::map_sqlx_err;

/// Double-quote a SQLite identifier, escaping an embedded `"` as `""`.
///
/// This is the only safe way to splice a table name into a `PRAGMA` call (which
/// cannot bind parameters). Example: `we"ird` → `"we""ird"`.
pub(crate) fn quote_ident(ident: &str) -> String {
    let mut out = String::with_capacity(ident.len() + 2);
    out.push('"');
    for ch in ident.chars() {
        if ch == '"' {
            out.push('"');
        }
        out.push(ch);
    }
    out.push('"');
    out
}

/// Map an introspection failure to [`CoreError::Introspection`] so the UI can
/// tell a metadata-load failure apart from a user-query failure.
fn introspection_err(err: CoreError) -> CoreError {
    CoreError::Introspection(err.to_string())
}

/// List the databases visible to this connection: `main` plus any `ATTACH`ed
/// databases (`PRAGMA database_list` rows: `seq, name, file`). SQLite has no
/// notion of system databases or offline state, so each is `is_system: false`,
/// `state_desc: "ONLINE"`.
pub(crate) async fn list_databases(
    conn: &mut SqliteConnection,
) -> Result<Vec<DatabaseInfo>, CoreError> {
    let rows = sqlx::query("PRAGMA database_list")
        .fetch_all(conn)
        .await
        .map_err(map_sqlx_err)
        .map_err(introspection_err)?;

    let mut out = Vec::with_capacity(rows.len().max(1));
    for row in &rows {
        // Column 1 is the database name (`main`, `temp`, or an attached alias).
        let name: String = row.try_get("name").map_err(map_sqlx_err)?;
        out.push(DatabaseInfo {
            name,
            is_system: false,
            state_desc: "ONLINE".to_string(),
        });
    }
    // `main` is always present in practice; guard against an empty result so the
    // tree always has a root to expand.
    if out.is_empty() {
        out.push(DatabaseInfo {
            name: "main".to_string(),
            is_system: false,
            state_desc: "ONLINE".to_string(),
        });
    }
    Ok(out)
}

/// SQLite has no schema level — return empty. The driver advertises
/// `capabilities.schemas = false`, so the UI never asks beyond this.
pub(crate) async fn list_schemas(
    _conn: &mut SqliteConnection,
    _database: &str,
) -> Result<Vec<SchemaInfo>, CoreError> {
    Ok(Vec::new())
}

/// List tables and views from `sqlite_master`, excluding SQLite's internal
/// `sqlite_*` objects. The `schema` of each is the empty string (no schema
/// level).
pub(crate) async fn list_tables(
    conn: &mut SqliteConnection,
    _database: &str,
    _schema: &str,
) -> Result<Vec<TableInfo>, CoreError> {
    let rows = sqlx::query(
        "SELECT name, type FROM sqlite_master \
         WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite_%' \
         ORDER BY name",
    )
    .fetch_all(conn)
    .await
    .map_err(map_sqlx_err)
    .map_err(introspection_err)?;

    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        let name: String = row.try_get("name").map_err(map_sqlx_err)?;
        let kind_str: String = row.try_get("type").map_err(map_sqlx_err)?;
        let kind = if kind_str.eq_ignore_ascii_case("view") {
            TableKind::View
        } else {
            TableKind::Table
        };
        out.push(TableInfo {
            schema: String::new(),
            name,
            kind,
        });
    }
    Ok(out)
}

/// List a table's columns via `PRAGMA table_info("<table>")`, mapping the
/// `cid, name, type, notnull, dflt_value, pk` rows to [`ColumnInfo`].
pub(crate) async fn list_columns(
    conn: &mut SqliteConnection,
    _database: &str,
    _schema: &str,
    table: &str,
) -> Result<Vec<ColumnInfo>, CoreError> {
    // PRAGMA arguments can't be bound; quote the identifier so the name can't
    // break out of the call.
    let sql = format!("PRAGMA table_info({})", quote_ident(table));
    let rows = sqlx::query(&sql)
        .fetch_all(conn)
        .await
        .map_err(map_sqlx_err)
        .map_err(introspection_err)?;

    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        // PRAGMA table_info columns: cid (i64), name (text), type (text),
        // notnull (i64 0/1), dflt_value (nullable), pk (i64; 0 = not a PK,
        // >0 = its 1-based position in the primary key).
        let cid: i64 = row.try_get("cid").map_err(map_sqlx_err)?;
        let name: String = row.try_get("name").map_err(map_sqlx_err)?;
        let data_type: String = row.try_get("type").map_err(map_sqlx_err)?;
        let notnull: i64 = row.try_get("notnull").map_err(map_sqlx_err)?;
        let pk: i64 = row.try_get("pk").map_err(map_sqlx_err)?;

        out.push(ColumnInfo {
            name,
            ordinal: cid as i32,
            data_type,
            nullable: notnull == 0,
            is_primary_key: pk > 0,
            max_length: None,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_ident_wraps_in_double_quotes() {
        assert_eq!(quote_ident("users"), "\"users\"");
        assert_eq!(quote_ident("My Table"), "\"My Table\"");
    }

    #[test]
    fn quote_ident_escapes_embedded_double_quote() {
        // The injection vector: a `"` must be doubled so it cannot close the
        // quoted identifier early.
        assert_eq!(quote_ident("a\"b"), "\"a\"\"b\"");
        assert_eq!(quote_ident("\"\""), "\"\"\"\"\"\"");
    }

    #[test]
    fn quote_ident_passes_through_other_chars() {
        assert_eq!(
            quote_ident("a'); DROP TABLE x;--"),
            "\"a'); DROP TABLE x;--\""
        );
        assert_eq!(quote_ident(""), "\"\"");
    }
}
