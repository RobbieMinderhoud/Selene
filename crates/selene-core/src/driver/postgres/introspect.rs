//! Schema introspection for Postgres.
//!
//! Postgres has a real schema level (database → schema → table → column), so
//! `capabilities.schemas` is `true`. The four levels read the standard catalogs:
//! - databases ← `current_database()` (see [`list_databases`] for why only the
//!   connected database is returned);
//! - schemas ← `pg_namespace` (user schemas only);
//! - tables/views ← `information_schema.tables`;
//! - columns ← `information_schema.columns` joined to the PK constraint views.
//!
//! ## SQL-injection safety
//! Every user-supplied value (schema, table) is passed as a bound `$N` parameter,
//! never interpolated. [`quote_ident`] exists for the rare case where an
//! identifier must be spliced (it double-quotes and doubles embedded `"`), but
//! the queries here bind everything, so it is currently used only by the import
//! path.

use sqlx::postgres::PgConnection;
use sqlx::Row as _;

use crate::error::CoreError;
use crate::introspect::{ColumnInfo, DatabaseInfo, SchemaInfo, TableInfo, TableKind};

use super::error::map_sqlx_err;

/// Double-quote a Postgres identifier, escaping an embedded `"` as `""`.
///
/// The safe way to splice an identifier into SQL when it cannot be a bound
/// parameter (e.g. a table name in `CREATE TABLE`). Example: `we"ird` →
/// `"we""ird"`.
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

/// List the databases visible to this connection.
///
/// **Returns only the currently-connected database**, not every database on the
/// cluster. A single Postgres connection is bound to one database for its whole
/// lifetime: it cannot switch databases (there is no `USE`) and cannot introspect
/// the contents of another database over the same connection. Listing all of
/// `pg_database` and then trying to introspect each would therefore be broken —
/// every drill-down past the connected database would fail. Browsing another
/// database means opening a *separate* connection to it; a future frontend
/// "reconnect to switch database" affordance can build on that. Until then we
/// surface exactly the one database the user is actually attached to.
///
/// The connected database has no meaningful "system" flag here and is always
/// online (we are connected to it), so `is_system: false`, `state_desc: "ONLINE"`.
pub(crate) async fn list_databases(
    conn: &mut PgConnection,
) -> Result<Vec<DatabaseInfo>, CoreError> {
    let row = sqlx::query("SELECT current_database() AS name")
        .fetch_one(conn)
        .await
        .map_err(map_sqlx_err)
        .map_err(introspection_err)?;
    let name: String = row.try_get("name").map_err(map_sqlx_err)?;
    Ok(vec![DatabaseInfo {
        name,
        is_system: false,
        state_desc: "ONLINE".to_string(),
    }])
}

/// List user schemas in the connected database. The `database` argument is
/// accepted for trait-shape parity but is implicitly the connected database (a
/// Postgres connection cannot introspect another). System schemas (`pg_*` and
/// `information_schema`) are excluded.
pub(crate) async fn list_schemas(
    conn: &mut PgConnection,
    _database: &str,
) -> Result<Vec<SchemaInfo>, CoreError> {
    // `pg\_%` escapes the LIKE wildcard so it matches a literal underscore: this
    // drops `pg_catalog`, `pg_toast`, `pg_temp_*`, etc., and the separate clause
    // drops `information_schema`.
    let rows = sqlx::query(
        "SELECT nspname \
         FROM pg_namespace \
         WHERE nspname NOT LIKE 'pg\\_%' AND nspname <> 'information_schema' \
         ORDER BY nspname",
    )
    .fetch_all(conn)
    .await
    .map_err(map_sqlx_err)
    .map_err(introspection_err)?;

    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        let name: String = row.try_get("nspname").map_err(map_sqlx_err)?;
        out.push(SchemaInfo { name });
    }
    Ok(out)
}

/// List tables and views in `schema` (of the connected database). The schema
/// name is bound as `$1`.
pub(crate) async fn list_tables(
    conn: &mut PgConnection,
    _database: &str,
    schema: &str,
) -> Result<Vec<TableInfo>, CoreError> {
    let rows = sqlx::query(
        "SELECT table_name, table_type \
         FROM information_schema.tables \
         WHERE table_schema = $1 \
         ORDER BY table_name",
    )
    .bind(schema)
    .fetch_all(conn)
    .await
    .map_err(map_sqlx_err)
    .map_err(introspection_err)?;

    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        let name: String = row.try_get("table_name").map_err(map_sqlx_err)?;
        let table_type: String = row.try_get("table_type").map_err(map_sqlx_err)?;
        // information_schema reports 'BASE TABLE' or 'VIEW' (and 'LOCAL TEMPORARY',
        // 'FOREIGN' — treated as tables).
        let kind = if table_type.eq_ignore_ascii_case("VIEW") {
            TableKind::View
        } else {
            TableKind::Table
        };
        out.push(TableInfo {
            schema: schema.to_string(),
            name,
            kind,
        });
    }
    Ok(out)
}

/// List columns of `schema`.`table` (in the connected database), including
/// primary-key flags. Both the schema and table names are bound as `$1`/`$2`.
///
/// Primary-key membership is detected by joining the table's columns against the
/// key-column-usage view filtered to PRIMARY KEY constraints — the same shape as
/// the mssql driver's query, against `information_schema`.
pub(crate) async fn list_columns(
    conn: &mut PgConnection,
    _database: &str,
    schema: &str,
    table: &str,
) -> Result<Vec<ColumnInfo>, CoreError> {
    // The PK subquery resolves constraint names from TABLE_CONSTRAINTS (type
    // 'PRIMARY KEY') and the participating columns from KEY_COLUMN_USAGE; a column
    // is a PK member if its name appears in that set for this table.
    let rows = sqlx::query(
        "SELECT \
             c.column_name, \
             c.ordinal_position, \
             c.data_type, \
             c.is_nullable, \
             c.character_maximum_length, \
             CASE WHEN pk.column_name IS NOT NULL THEN true ELSE false END AS is_pk \
         FROM information_schema.columns AS c \
         LEFT JOIN ( \
             SELECT kcu.column_name \
             FROM information_schema.table_constraints AS tc \
             JOIN information_schema.key_column_usage AS kcu \
               ON tc.constraint_name = kcu.constraint_name \
              AND tc.constraint_schema = kcu.constraint_schema \
             WHERE tc.constraint_type = 'PRIMARY KEY' \
               AND tc.table_schema = $1 \
               AND tc.table_name = $2 \
         ) AS pk ON pk.column_name = c.column_name \
         WHERE c.table_schema = $1 AND c.table_name = $2 \
         ORDER BY c.ordinal_position",
    )
    .bind(schema)
    .bind(table)
    .fetch_all(conn)
    .await
    .map_err(map_sqlx_err)
    .map_err(introspection_err)?;

    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        let name: String = row.try_get("column_name").map_err(map_sqlx_err)?;
        // ordinal_position is reported as `int` by information_schema.
        let ordinal: i32 = row.try_get("ordinal_position").map_err(map_sqlx_err)?;
        let data_type: String = row.try_get("data_type").map_err(map_sqlx_err)?;
        let is_nullable: String = row.try_get("is_nullable").map_err(map_sqlx_err)?;
        // character_maximum_length is `int` and NULL for non-character types.
        let max_length: Option<i32> = row
            .try_get("character_maximum_length")
            .map_err(map_sqlx_err)?;
        let is_pk: bool = row.try_get("is_pk").map_err(map_sqlx_err)?;

        out.push(ColumnInfo {
            name,
            ordinal,
            data_type,
            nullable: is_nullable.eq_ignore_ascii_case("YES"),
            is_primary_key: is_pk,
            max_length,
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
