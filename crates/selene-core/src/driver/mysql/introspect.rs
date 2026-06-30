//! Schema introspection for MySQL.
//!
//! MySQL has **no schema level**: a "schema" *is* a database (the words are
//! synonyms), so `capabilities.schemas` is `false`. The levels collapse to
//! database → table → column:
//! - databases ← `information_schema.schemata` (user databases only);
//! - schemas ← always empty (the UI skips this level when `schemas == false`);
//! - tables/views ← `information_schema.tables` filtered by `table_schema`;
//! - columns ← `information_schema.columns` joined to the PK constraint views.
//!
//! Because there is no schema level, `list_tables`/`list_columns` ignore the
//! `schema` argument (the frontend may pass a placeholder when `schemas` is off)
//! and filter on the **database** name instead.
//!
//! ## SQL-injection safety
//! Every user-supplied value (database, table) is passed as a bound `?`
//! parameter, never interpolated. [`quote_ident`] exists for the import/`USE`
//! paths where an identifier must be spliced (it backtick-quotes and doubles an
//! embedded backtick).

use sqlx::mysql::MySqlConnection;
use sqlx::Row as _;

use crate::error::CoreError;
use crate::introspect::{ColumnInfo, DatabaseInfo, SchemaInfo, TableInfo, TableKind};

use super::error::map_sqlx_err;

/// The MySQL system databases, excluded from the user-facing database list.
const SYSTEM_DATABASES: [&str; 4] = ["mysql", "information_schema", "performance_schema", "sys"];

/// Backtick-quote a MySQL identifier, escaping an embedded backtick by doubling
/// it.
///
/// The safe way to splice an identifier into SQL when it cannot be a bound
/// parameter (e.g. a table name in `CREATE TABLE`, a database in `USE`). Example:
/// `` we`ird `` → `` `we``ird` ``.
pub(crate) fn quote_ident(ident: &str) -> String {
    let mut out = String::with_capacity(ident.len() + 2);
    out.push('`');
    for ch in ident.chars() {
        if ch == '`' {
            out.push('`');
        }
        out.push(ch);
    }
    out.push('`');
    out
}

/// Map an introspection failure to [`CoreError::Introspection`] so the UI can
/// tell a metadata-load failure apart from a user-query failure.
fn introspection_err(err: CoreError) -> CoreError {
    CoreError::Introspection(err.to_string())
}

/// List the user databases on the server (the MySQL system databases are
/// excluded). MySQL *can* switch databases on a live connection (`USE`), so —
/// unlike Postgres — every user database is listed, not just the connected one.
///
/// MySQL has no per-database availability state, so `state_desc` is always
/// `"ONLINE"`; the excluded system databases mean `is_system` is always `false`.
pub(crate) async fn list_databases(
    conn: &mut MySqlConnection,
) -> Result<Vec<DatabaseInfo>, CoreError> {
    // `CAST(... AS CHAR)`: MySQL 8's information_schema string columns carry a
    // binary collation and come back to sqlx as `VARBINARY` (which does not decode
    // into `String`); casting to CHAR yields a proper character type.
    let rows = sqlx::query(
        "SELECT CAST(schema_name AS CHAR) \
         FROM information_schema.schemata \
         WHERE schema_name NOT IN (?, ?, ?, ?) \
         ORDER BY schema_name",
    )
    .bind(SYSTEM_DATABASES[0])
    .bind(SYSTEM_DATABASES[1])
    .bind(SYSTEM_DATABASES[2])
    .bind(SYSTEM_DATABASES[3])
    .fetch_all(conn)
    .await
    .map_err(map_sqlx_err)
    .map_err(introspection_err)?;

    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        // Read by ordinal: MySQL's information_schema reports column names in
        // UPPERCASE (`SCHEMA_NAME`), and sqlx's by-name lookup is case-sensitive,
        // so positional access avoids depending on the server's casing.
        let name: String = row.try_get(0).map_err(map_sqlx_err)?;
        out.push(DatabaseInfo {
            name,
            is_system: false,
            state_desc: "ONLINE".to_string(),
        });
    }
    Ok(out)
}

/// MySQL has no schema level — a "schema" is a database. Always returns an empty
/// list; the UI skips this level because `capabilities.schemas` is `false`.
pub(crate) async fn list_schemas(
    _conn: &mut MySqlConnection,
    _database: &str,
) -> Result<Vec<SchemaInfo>, CoreError> {
    Ok(Vec::new())
}

/// List tables and views in `database`. The `schema` argument is ignored (MySQL
/// has no schema level); the database name is bound as the `table_schema` filter.
pub(crate) async fn list_tables(
    conn: &mut MySqlConnection,
    database: &str,
    _schema: &str,
) -> Result<Vec<TableInfo>, CoreError> {
    // CAST string columns to CHAR (see `list_databases`): MySQL 8's
    // information_schema reports them as VARBINARY otherwise.
    let rows = sqlx::query(
        "SELECT CAST(table_name AS CHAR), CAST(table_type AS CHAR) \
         FROM information_schema.tables \
         WHERE table_schema = ? \
         ORDER BY table_name",
    )
    .bind(database)
    .fetch_all(conn)
    .await
    .map_err(map_sqlx_err)
    .map_err(introspection_err)?;

    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        // Positional reads (see `list_databases`): information_schema column names
        // are UPPERCASE and sqlx's by-name lookup is case-sensitive.
        let name: String = row.try_get(0).map_err(map_sqlx_err)?;
        let table_type: String = row.try_get(1).map_err(map_sqlx_err)?;
        // information_schema reports 'BASE TABLE' or 'VIEW' (and 'SYSTEM VIEW',
        // treated as a view).
        let kind = if table_type.to_ascii_uppercase().contains("VIEW") {
            TableKind::View
        } else {
            TableKind::Table
        };
        // `schema` on TableInfo carries the database name for MySQL (its sole
        // namespace level), keeping the field populated for the UI.
        out.push(TableInfo {
            schema: database.to_string(),
            name,
            kind,
        });
    }
    Ok(out)
}

/// List columns of `table` in `database`, including primary-key flags. The
/// `schema` argument is ignored (no schema level); the database and table names
/// are bound as `?` parameters.
pub(crate) async fn list_columns(
    conn: &mut MySqlConnection,
    database: &str,
    _schema: &str,
    table: &str,
) -> Result<Vec<ColumnInfo>, CoreError> {
    // The PK subquery resolves the PRIMARY KEY constraint's columns from
    // TABLE_CONSTRAINTS + KEY_COLUMN_USAGE; a column is a PK member if its name
    // appears in that set for this table. All filters bind on the database
    // (table_schema) and table name.
    // CAST the string columns to CHAR (see `list_databases`): MySQL 8's
    // information_schema reports them as VARBINARY, which does not decode into
    // `String`. The numeric columns and the PK join (binary-to-binary comparison)
    // are left as-is.
    let rows = sqlx::query(
        "SELECT \
             CAST(c.column_name AS CHAR), \
             c.ordinal_position, \
             CAST(c.data_type AS CHAR), \
             CAST(c.is_nullable AS CHAR), \
             c.character_maximum_length, \
             CASE WHEN pk.column_name IS NOT NULL THEN 1 ELSE 0 END AS is_pk \
         FROM information_schema.columns AS c \
         LEFT JOIN ( \
             SELECT kcu.column_name \
             FROM information_schema.table_constraints AS tc \
             JOIN information_schema.key_column_usage AS kcu \
               ON tc.constraint_name = kcu.constraint_name \
              AND tc.constraint_schema = kcu.constraint_schema \
              AND tc.table_name = kcu.table_name \
             WHERE tc.constraint_type = 'PRIMARY KEY' \
               AND tc.table_schema = ? \
               AND tc.table_name = ? \
         ) AS pk ON pk.column_name = c.column_name \
         WHERE c.table_schema = ? AND c.table_name = ? \
         ORDER BY c.ordinal_position",
    )
    .bind(database)
    .bind(table)
    .bind(database)
    .bind(table)
    .fetch_all(conn)
    .await
    .map_err(map_sqlx_err)
    .map_err(introspection_err)?;

    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        // Positional reads (see `list_databases`): information_schema column names
        // are UPPERCASE and sqlx's by-name lookup is case-sensitive. The SELECT
        // order is: column_name, ordinal_position, data_type, is_nullable,
        // character_maximum_length, is_pk.
        let name: String = row.try_get(0).map_err(map_sqlx_err)?;
        // ordinal_position is an unsigned bigint in MySQL's information_schema.
        let ordinal: u64 = row.try_get(1).map_err(map_sqlx_err)?;
        let data_type: String = row.try_get(2).map_err(map_sqlx_err)?;
        let is_nullable: String = row.try_get(3).map_err(map_sqlx_err)?;
        // character_maximum_length is a (possibly NULL) bigint in MySQL.
        let max_length: Option<i64> = row.try_get(4).map_err(map_sqlx_err)?;
        // The CASE expression yields a signed integer (1/0).
        let is_pk: i64 = row.try_get(5).map_err(map_sqlx_err)?;

        out.push(ColumnInfo {
            name,
            // ColumnInfo's ordinal is i32; positions are small, so the cast is safe.
            ordinal: ordinal as i32,
            data_type,
            nullable: is_nullable.eq_ignore_ascii_case("YES"),
            is_primary_key: is_pk != 0,
            // max_length on ColumnInfo is i32; character lengths fit.
            max_length: max_length.map(|n| n as i32),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_ident_wraps_in_backticks() {
        assert_eq!(quote_ident("users"), "`users`");
        assert_eq!(quote_ident("My Table"), "`My Table`");
    }

    #[test]
    fn quote_ident_escapes_embedded_backtick() {
        // The injection vector: a backtick must be doubled so it cannot close the
        // quoted identifier early.
        assert_eq!(quote_ident("a`b"), "`a``b`");
        // Two backticks => each doubled (4) => wrapped (6 total).
        assert_eq!(quote_ident("``"), "``````");
    }

    #[test]
    fn quote_ident_passes_through_other_chars() {
        assert_eq!(
            quote_ident("a'); DROP TABLE x;--"),
            "`a'); DROP TABLE x;--`"
        );
        assert_eq!(quote_ident(""), "``");
    }
}
