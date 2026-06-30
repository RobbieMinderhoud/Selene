//! SQL Server database backup & restore.
//!
//! `BACKUP`/`RESTORE` run **server-side**: the disk path is on the database
//! server's filesystem, and the operation is a single non-row-returning
//! statement (so it goes through the plain `simple_query` batch path, like the
//! other DB-management DDL in [`super`]). Progress is *not* available from the
//! statement itself (tiberius drops the server's `STATS` info messages), so the
//! command layer polls [`request_percent_complete`] on a **separate** connection
//! while the backup/restore runs on this one.
//!
//! ## SQL-injection safety
//! Database names are bracket-quoted with [`quote_ident`]. File paths and
//! logical file names cannot be bound parameters here (`BACKUP`/`RESTORE` take
//! them as literals, and `RESTORE FILELISTONLY` accepts no parameters at all),
//! so they are spliced as N-quoted string literals with embedded `'` doubled by
//! [`quote_literal`] — the standard T-SQL escaping. Every other user value
//! (the target database name in `database_files`, the session id in
//! `request_percent_complete`) is a bound parameter.

use tiberius::ToSql;

use crate::backup::{
    BackupFile, BackupOptions, DbFile, DefaultDirs, FileMove, RestoreOptions, ServerDirEntry,
};
use crate::error::CoreError;

use super::error::map_tiberius_err;
use super::introspect::quote_ident;
use super::stream::TiberiusClient;

/// Escape a string for use as a T-SQL single-quoted literal: double any embedded
/// `'`. Callers wrap the result themselves (we return the inner text only), e.g.
/// `format!("N'{}'", quote_literal(path))`.
fn quote_literal(s: &str) -> String {
    s.replace('\'', "''")
}

/// Run a non-row-returning statement (or batch) and drain its result stream to
/// complete the round-trip — the pattern used by every DB-management call.
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

/// Run a query with bound params and collect the first result set into memory
/// (these result sets are tiny — file lists, a single scalar).
async fn fetch_rows(
    client: &mut TiberiusClient,
    sql: &str,
    params: &[&dyn ToSql],
) -> Result<Vec<tiberius::Row>, CoreError> {
    let stream = client.query(sql, params).await.map_err(map_tiberius_err)?;
    stream.into_first_result().await.map_err(map_tiberius_err)
}

/// `BACKUP DATABASE [database] TO DISK = N'to_path' WITH …`.
///
/// Uses `FORMAT, INIT` so the target `.bak` is (re)written as a fresh single-
/// backup media set rather than appended to. Optionally adds `COMPRESSION` /
/// `CHECKSUM`, and runs `RESTORE VERIFYONLY` afterwards when requested.
pub(crate) async fn backup_database(
    client: &mut TiberiusClient,
    database: &str,
    to_path: &str,
    opts: &BackupOptions,
) -> Result<(), CoreError> {
    let db = quote_ident(database);
    let path = quote_literal(to_path);
    let name = quote_literal(&format!("{database}-full"));

    let mut with = vec![
        "FORMAT".to_string(),
        "INIT".to_string(),
        format!("NAME = N'{name}'"),
        if opts.compression {
            "COMPRESSION".to_string()
        } else {
            "NO_COMPRESSION".to_string()
        },
    ];
    if opts.checksum {
        with.push("CHECKSUM".to_string());
    }

    let sql = format!(
        "BACKUP DATABASE {db} TO DISK = N'{path}' WITH {}",
        with.join(", ")
    );
    run_batch(client, &sql).await?;

    if opts.verify_after {
        let verify = if opts.checksum {
            format!("RESTORE VERIFYONLY FROM DISK = N'{path}' WITH CHECKSUM")
        } else {
            format!("RESTORE VERIFYONLY FROM DISK = N'{path}'")
        };
        run_batch(client, &verify).await?;
    }
    Ok(())
}

/// `RESTORE FILELISTONLY FROM DISK = N'from_path'` → the logical files in a
/// backup set.
pub(crate) async fn restore_filelist(
    client: &mut TiberiusClient,
    from_path: &str,
) -> Result<Vec<BackupFile>, CoreError> {
    let path = quote_literal(from_path);
    // FILELISTONLY accepts no parameters, so the path is a quoted literal.
    let sql = format!("RESTORE FILELISTONLY FROM DISK = N'{path}'");
    let rows = client
        .simple_query(sql)
        .await
        .map_err(map_tiberius_err)?
        .into_first_result()
        .await
        .map_err(map_tiberius_err)?;

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let logical_name: &str = row.get("LogicalName").unwrap_or("");
        let physical_name: &str = row.get("PhysicalName").unwrap_or("");
        let file_type: &str = row.get("Type").unwrap_or("");
        out.push(BackupFile {
            logical_name: logical_name.to_string(),
            physical_name: physical_name.to_string(),
            file_type: file_type.to_string(),
        });
    }
    Ok(out)
}

/// Current physical files of `database` from `sys.master_files`.
pub(crate) async fn database_files(
    client: &mut TiberiusClient,
    database: &str,
) -> Result<Vec<DbFile>, CoreError> {
    // DB_ID resolves the name even for an offline database; the name is bound.
    let sql = "SELECT name, physical_name, type_desc \
               FROM sys.master_files \
               WHERE database_id = DB_ID(@P1) \
               ORDER BY file_id";
    let rows = fetch_rows(client, sql, &[&database]).await?;

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let name: &str = row.get(0).unwrap_or("");
        let physical_name: &str = row.get(1).unwrap_or("");
        let type_desc: &str = row.get(2).unwrap_or("");
        out.push(DbFile {
            logical_name: name.to_string(),
            physical_name: physical_name.to_string(),
            is_log: type_desc.eq_ignore_ascii_case("LOG"),
        });
    }
    Ok(out)
}

/// Server default data/log directories (empty strings when unset on this
/// instance — the caller derives directories from the target database instead).
pub(crate) async fn default_file_dirs(
    client: &mut TiberiusClient,
) -> Result<DefaultDirs, CoreError> {
    let sql = "SELECT \
               CAST(SERVERPROPERTY('InstanceDefaultDataPath') AS nvarchar(4000)) AS data_dir, \
               CAST(SERVERPROPERTY('InstanceDefaultLogPath') AS nvarchar(4000)) AS log_dir";
    let rows = fetch_rows(client, sql, &[]).await?;
    let (data, log) = match rows.first() {
        Some(row) => (
            row.get::<&str, _>(0).unwrap_or("").to_string(),
            row.get::<&str, _>(1).unwrap_or("").to_string(),
        ),
        None => (String::new(), String::new()),
    };
    Ok(DefaultDirs { data, log })
}

/// The server's default backup directory, for pre-filling the destination and
/// seeding the server file browser. Falls back to the default data directory,
/// then an empty string, when the instance does not expose a backup path.
pub(crate) async fn default_backup_dir(client: &mut TiberiusClient) -> Result<String, CoreError> {
    let sql = "SELECT COALESCE( \
               CAST(SERVERPROPERTY('InstanceDefaultBackupPath') AS nvarchar(4000)), \
               CAST(SERVERPROPERTY('InstanceDefaultDataPath') AS nvarchar(4000)), \
               '')";
    let rows = fetch_rows(client, sql, &[]).await?;
    Ok(rows
        .first()
        .and_then(|r| r.get::<&str, _>(0))
        .unwrap_or("")
        .to_string())
}

/// List the immediate children of server directory `path` via `xp_dirtree`
/// (`@path, depth = 1, include_files = 1`). Returns one [`ServerDirEntry`] per
/// child (names only); a non-existent path yields an empty list, not an error.
///
/// `xp_dirtree` returns columns `subdirectory` (the child's name), `depth`, and
/// `file` (1 for a file, 0 for a directory). The path is bound, not spliced.
pub(crate) async fn list_server_dir(
    client: &mut TiberiusClient,
    path: &str,
) -> Result<Vec<ServerDirEntry>, CoreError> {
    let rows = fetch_rows(client, "EXEC master.sys.xp_dirtree @P1, 1, 1", &[&path]).await?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let name: &str = match row.try_get("subdirectory").map_err(map_tiberius_err)? {
            Some(n) => n,
            None => continue,
        };
        // `file` is 1 for files, 0 for directories.
        let is_file: i32 = row.try_get("file").map_err(map_tiberius_err)?.unwrap_or(0);
        out.push(ServerDirEntry {
            name: name.to_string(),
            is_dir: is_file == 0,
        });
    }
    Ok(out)
}

/// Restore the backup at `from_path` over the existing database `target`.
///
/// Sequence: take `target` single-user (kicking other sessions), `RESTORE …
/// WITH REPLACE, RECOVERY` relocating files per `moves`, then return it to
/// multi-user. The multi-user step runs even when the restore fails, so a failed
/// restore never leaves the database stuck in single-user mode; the original
/// restore error is then surfaced.
pub(crate) async fn restore_database(
    client: &mut TiberiusClient,
    target: &str,
    from_path: &str,
    moves: &[FileMove],
    opts: &RestoreOptions,
) -> Result<(), CoreError> {
    let db = quote_ident(target);
    let path = quote_literal(from_path);

    // Exclusive access. ROLLBACK IMMEDIATE terminates other sessions at once
    // (rather than waiting), so this does not time out and needs no lock-timeout
    // handling. Run from `master` so this connection is not itself "using" the
    // target database.
    run_batch(
        client,
        &format!("USE master; ALTER DATABASE {db} SET SINGLE_USER WITH ROLLBACK IMMEDIATE"),
    )
    .await?;

    let mut with = vec!["REPLACE".to_string(), "RECOVERY".to_string()];
    if opts.checksum {
        with.push("CHECKSUM".to_string());
    }
    for m in moves {
        with.push(format!(
            "MOVE N'{}' TO N'{}'",
            quote_literal(&m.logical_name),
            quote_literal(&m.to_path)
        ));
    }
    let restore_sql = format!(
        "USE master; RESTORE DATABASE {db} FROM DISK = N'{path}' WITH {}",
        with.join(", ")
    );

    let result = run_batch(client, &restore_sql).await;

    // Always restore multi-user access: on success this is a harmless no-op
    // (the restored db is typically multi-user already); on failure it frees the
    // database from the single-user state set above.
    let _ = run_batch(
        client,
        &format!("USE master; ALTER DATABASE {db} SET MULTI_USER"),
    )
    .await;

    result
}

/// `SELECT @@SPID` — this connection's server session id.
pub(crate) async fn current_session_id(client: &mut TiberiusClient) -> Result<i32, CoreError> {
    let rows = fetch_rows(client, "SELECT CAST(@@SPID AS int)", &[]).await?;
    let spid = rows
        .first()
        .and_then(|r| r.get::<i32, _>(0))
        .ok_or_else(|| CoreError::Query("could not read @@SPID".into()))?;
    Ok(spid)
}

/// `percent_complete` of the request on session `spid`, or `None` if no request
/// is currently active for it. Requires `VIEW SERVER STATE`.
pub(crate) async fn request_percent_complete(
    client: &mut TiberiusClient,
    spid: i32,
) -> Result<Option<f32>, CoreError> {
    let sql = "SELECT percent_complete FROM sys.dm_exec_requests WHERE session_id = @P1";
    let rows = fetch_rows(client, sql, &[&spid]).await?;
    Ok(rows.first().and_then(|r| r.get::<f32, _>(0)))
}

/// `KILL <spid>` — best-effort termination of a running backup/restore session.
pub(crate) async fn kill_session(client: &mut TiberiusClient, spid: i32) -> Result<(), CoreError> {
    // `spid` is a validated i32 (never user text), so splicing it is injection-safe.
    run_batch(client, &format!("KILL {spid}")).await
}

/// Best-effort delete of a single server-side file via `xp_cmdshell` — the only
/// cross-platform way to remove an arbitrary file over a SQL connection (the
/// folder/date-based `xp_delete_file` cannot target one file safely, and OLE
/// automation is Windows-only).
///
/// **We never enable `xp_cmdshell`.** When it is disabled (the default) the
/// server raises error 15281, which surfaces here as a `CoreError::Query`; the
/// caller treats a delete failure as non-fatal (the restore already succeeded).
///
/// The shell command is chosen from the path's separator (`\` ⇒ Windows `del`,
/// otherwise POSIX `rm`). The path is escaped for the shell, then the whole
/// command is escaped again as a T-SQL string literal.
pub(crate) async fn delete_server_file(
    client: &mut TiberiusClient,
    path: &str,
) -> Result<(), CoreError> {
    let cmd = if path.contains('\\') {
        // Windows filenames cannot contain `"`, so double-quoting is sufficient.
        format!("del /f /q \"{path}\"")
    } else {
        // POSIX: single-quote the path, escaping any embedded `'` as `'\''`.
        let escaped = path.replace('\'', "'\\''");
        format!("rm -f '{escaped}'")
    };
    let sql = format!("EXEC master.sys.xp_cmdshell N'{}'", quote_literal(&cmd));
    run_batch(client, &sql).await
}
