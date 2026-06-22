//! Schema introspection via SQL Server catalog views.
//!
//! Introspection is lazy per level (databases → schemas → tables → columns).
//! Each query targets a specific database using **three-part names**
//! (`[db].sys.schemas`, `[db].INFORMATION_SCHEMA.TABLES`, …).
//!
//! ## SQL-injection safety
//! A database name cannot be a bound parameter when it is part of a three-part
//! identifier, so it is interpolated textually. [`quote_ident`] makes that safe
//! by bracket-quoting and doubling any `]` — the standard T-SQL escaping, and
//! exactly what `QUOTENAME` does. Every other user-supplied value (schema and
//! table filters) is passed as a bound `@P1`/`@P2` parameter, never
//! interpolated.

use tiberius::ToSql;

use crate::error::CoreError;
use crate::introspect::{ColumnInfo, DatabaseInfo, SchemaInfo, TableInfo, TableKind};

use super::error::map_tiberius_err;
use super::stream::TiberiusClient;

/// Bracket-quote a SQL Server identifier, escaping embedded `]` as `]]`.
///
/// This mirrors `QUOTENAME(name, '[')` and is the only safe way to splice a
/// database name into a three-part identifier. Example: `my]db` → `[my]]db]`.
pub(crate) fn quote_ident(ident: &str) -> String {
    let mut out = String::with_capacity(ident.len() + 2);
    out.push('[');
    for ch in ident.chars() {
        if ch == ']' {
            out.push(']');
        }
        out.push(ch);
    }
    out.push(']');
    out
}

/// Run a query with bound params and collect every row of the first result set
/// into memory. Introspection result sets are small (object lists), so full
/// buffering is fine here — unlike the user-facing streaming path.
async fn fetch_rows(
    client: &mut TiberiusClient,
    sql: &str,
    params: &[&dyn ToSql],
) -> Result<Vec<tiberius::Row>, CoreError> {
    let stream = client.query(sql, params).await.map_err(map_tiberius_err)?;
    stream.into_first_result().await.map_err(map_tiberius_err)
}

/// Map an introspection failure: these are reported as `Introspection` errors
/// regardless of the underlying tiberius cause, so the UI can distinguish a
/// metadata-load failure from a user-query failure.
fn introspection_err(err: CoreError) -> CoreError {
    CoreError::Introspection(err.to_string())
}

/// List all databases on the server. System databases are `master`, `tempdb`,
/// `model`, `msdb` — identified by `database_id <= 4`.
pub(crate) async fn list_databases(
    client: &mut TiberiusClient,
) -> Result<Vec<DatabaseInfo>, CoreError> {
    // `HAS_DBACCESS` filters out databases the login cannot open, avoiding
    // noise the user could never expand anyway. We additionally keep OFFLINE
    // databases (`state = 6`), for which `HAS_DBACCESS` returns NULL, so the UI
    // can show them and offer "bring online".
    let sql = "SELECT name, database_id, state_desc \
               FROM sys.databases \
               WHERE HAS_DBACCESS(name) = 1 OR state = 6 \
               ORDER BY name";

    let rows = fetch_rows(client, sql, &[])
        .await
        .map_err(introspection_err)?;

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let name: &str = row
            .try_get(0)
            .map_err(map_tiberius_err)?
            .ok_or_else(|| CoreError::Introspection("database name was NULL".into()))?;
        let database_id: i32 = row
            .try_get(1)
            .map_err(map_tiberius_err)?
            .unwrap_or(i32::MAX);
        let state_desc: &str = row
            .try_get(2)
            .map_err(map_tiberius_err)?
            .unwrap_or("ONLINE");
        out.push(DatabaseInfo {
            name: name.to_string(),
            is_system: database_id <= 4,
            state_desc: state_desc.to_string(),
        });
    }
    Ok(out)
}

/// List user schemas in `database`. We exclude the built-in fixed-role and
/// system schemas (`sys`, `INFORMATION_SCHEMA`, `guest`, `db_*`) so the tree
/// shows what users care about.
pub(crate) async fn list_schemas(
    client: &mut TiberiusClient,
    database: &str,
) -> Result<Vec<SchemaInfo>, CoreError> {
    let db = quote_ident(database);
    // Drop the built-in system schemas by name: `sys`/`INFORMATION_SCHEMA`,
    // `guest`, and the fixed database-role schemas (`db_owner`, `db_datareader`,
    // …) matched by the `db_` prefix. `[_]` escapes the LIKE wildcard so it
    // matches a literal underscore.
    let sql = format!(
        "SELECT s.name \
         FROM {db}.sys.schemas AS s \
         WHERE s.name NOT IN ('sys', 'INFORMATION_SCHEMA', 'guest') \
           AND s.name NOT LIKE 'db[_]%' \
         ORDER BY s.name"
    );

    let rows = fetch_rows(client, &sql, &[])
        .await
        .map_err(introspection_err)?;

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        if let Some(name) = row.try_get::<&str, _>(0).map_err(map_tiberius_err)? {
            out.push(SchemaInfo {
                name: name.to_string(),
            });
        }
    }
    Ok(out)
}

/// List tables and views in `database`.`schema`. The schema name is bound as a
/// parameter; only the database name is interpolated (and quoted).
pub(crate) async fn list_tables(
    client: &mut TiberiusClient,
    database: &str,
    schema: &str,
) -> Result<Vec<TableInfo>, CoreError> {
    let db = quote_ident(database);
    let sql = format!(
        "SELECT TABLE_NAME, TABLE_TYPE \
         FROM {db}.INFORMATION_SCHEMA.TABLES \
         WHERE TABLE_SCHEMA = @P1 \
         ORDER BY TABLE_NAME"
    );

    let rows = fetch_rows(client, &sql, &[&schema])
        .await
        .map_err(introspection_err)?;

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let name: &str = match row.try_get(0).map_err(map_tiberius_err)? {
            Some(n) => n,
            None => continue,
        };
        let table_type: &str = row.try_get(1).map_err(map_tiberius_err)?.unwrap_or("");
        // INFORMATION_SCHEMA reports 'BASE TABLE' or 'VIEW'.
        let kind = if table_type.eq_ignore_ascii_case("VIEW") {
            TableKind::View
        } else {
            TableKind::Table
        };
        out.push(TableInfo {
            schema: schema.to_string(),
            name: name.to_string(),
            kind,
        });
    }
    Ok(out)
}

/// List columns of `database`.`schema`.`table`, including primary-key flags.
///
/// Primary-key membership is detected by joining the table's columns against
/// the key-column-usage view filtered to PRIMARY KEY constraints. Both the
/// schema and table names are bound parameters.
pub(crate) async fn list_columns(
    client: &mut TiberiusClient,
    database: &str,
    schema: &str,
    table: &str,
) -> Result<Vec<ColumnInfo>, CoreError> {
    let db = quote_ident(database);

    // The PK subquery resolves constraint names from TABLE_CONSTRAINTS (type
    // 'PRIMARY KEY') and the participating columns from KEY_COLUMN_USAGE. A
    // column is a PK member if its name appears in that set for this table.
    let sql = format!(
        "SELECT \
             c.COLUMN_NAME, \
             c.ORDINAL_POSITION, \
             c.DATA_TYPE, \
             c.IS_NULLABLE, \
             c.CHARACTER_MAXIMUM_LENGTH, \
             CAST(CASE WHEN pk.COLUMN_NAME IS NOT NULL THEN 1 ELSE 0 END AS int) AS IS_PK \
         FROM {db}.INFORMATION_SCHEMA.COLUMNS AS c \
         LEFT JOIN ( \
             SELECT kcu.COLUMN_NAME \
             FROM {db}.INFORMATION_SCHEMA.TABLE_CONSTRAINTS AS tc \
             JOIN {db}.INFORMATION_SCHEMA.KEY_COLUMN_USAGE AS kcu \
               ON tc.CONSTRAINT_NAME = kcu.CONSTRAINT_NAME \
              AND tc.CONSTRAINT_SCHEMA = kcu.CONSTRAINT_SCHEMA \
             WHERE tc.CONSTRAINT_TYPE = 'PRIMARY KEY' \
               AND tc.TABLE_SCHEMA = @P1 \
               AND tc.TABLE_NAME = @P2 \
         ) AS pk ON pk.COLUMN_NAME = c.COLUMN_NAME \
         WHERE c.TABLE_SCHEMA = @P1 AND c.TABLE_NAME = @P2 \
         ORDER BY c.ORDINAL_POSITION"
    );

    let rows = fetch_rows(client, &sql, &[&schema, &table])
        .await
        .map_err(introspection_err)?;

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let name: &str = match row.try_get(0).map_err(map_tiberius_err)? {
            Some(n) => n,
            None => continue,
        };
        // ORDINAL_POSITION is reported as an integer; INFORMATION_SCHEMA types
        // it as `int`, so read it as i32.
        let ordinal: i32 = row.try_get(1).map_err(map_tiberius_err)?.unwrap_or(0);
        let data_type: &str = row.try_get(2).map_err(map_tiberius_err)?.unwrap_or("");
        let is_nullable: &str = row.try_get(3).map_err(map_tiberius_err)?.unwrap_or("YES");
        // CHARACTER_MAXIMUM_LENGTH is `int` and NULL for non-character types.
        let max_length: Option<i32> = row.try_get(4).map_err(map_tiberius_err)?;
        let is_pk: i32 = row.try_get(5).map_err(map_tiberius_err)?.unwrap_or(0);

        out.push(ColumnInfo {
            name: name.to_string(),
            ordinal,
            data_type: data_type.to_string(),
            nullable: is_nullable.eq_ignore_ascii_case("YES"),
            is_primary_key: is_pk != 0,
            max_length,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_ident_wraps_in_brackets() {
        assert_eq!(quote_ident("master"), "[master]");
        assert_eq!(quote_ident("My Database"), "[My Database]");
    }

    #[test]
    fn quote_ident_escapes_closing_bracket() {
        // The classic injection vector: a `]` must be doubled so it cannot
        // terminate the quoted identifier early.
        assert_eq!(quote_ident("a]b"), "[a]]b]");
        assert_eq!(quote_ident("]]"), "[]]]]]");
        assert_eq!(quote_ident("ev]il"), "[ev]]il]");
    }

    #[test]
    fn quote_ident_passes_through_other_chars() {
        // Brackets are only special on the closing side; an opening bracket and
        // quotes/semicolons are harmless once wrapped.
        assert_eq!(quote_ident("we[ird"), "[we[ird]");
        assert_eq!(quote_ident("a';DROP TABLE x;--"), "[a';DROP TABLE x;--]");
    }

    #[test]
    fn quote_ident_handles_empty() {
        assert_eq!(quote_ident(""), "[]");
    }
}
