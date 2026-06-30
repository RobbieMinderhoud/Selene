//! Backup/restore value types and the (driver-neutral) restore file-relocation
//! planner.
//!
//! The actual `BACKUP`/`RESTORE` SQL lives in the driver
//! (`driver/mssql/backup.rs`); this module owns the **types** that cross the
//! driver boundary and the **pure** [`plan_moves`] logic that decides where each
//! file in a backup set should land when restoring it *over an existing target
//! database*. Keeping `plan_moves` here (no driver, no I/O) makes it unit-
//! testable with plain `cargo test`.

/// Options controlling a `BACKUP DATABASE`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BackupOptions {
    /// Use backup compression (`WITH COMPRESSION`). Ignored by editions that do
    /// not support it — SQL Server raises a clear error we surface verbatim.
    pub compression: bool,
    /// Compute and verify page checksums (`WITH CHECKSUM`).
    pub checksum: bool,
    /// After writing the backup, run `RESTORE VERIFYONLY` to confirm the media
    /// set is readable.
    pub verify_after: bool,
}

/// Options controlling a `RESTORE DATABASE`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RestoreOptions {
    /// Verify page checksums while restoring (`WITH CHECKSUM`).
    pub checksum: bool,
}

/// One logical file described by `RESTORE FILELISTONLY` — i.e. a file *inside* a
/// `.bak` media set. Surfaced to the frontend so the restore dialog can preview
/// what the backup contains.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BackupFile {
    /// The logical name the file had in the source database.
    pub logical_name: String,
    /// The physical path the file had when the backup was taken (informational;
    /// the restore relocates via [`FileMove`]).
    pub physical_name: String,
    /// File class as reported by `FILELISTONLY`: `"D"` data, `"L"` log, `"F"`
    /// full-text, `"S"` filestream.
    pub file_type: String,
}

impl BackupFile {
    /// Whether this is a data (rows) file (`Type = 'D'`).
    pub fn is_data(&self) -> bool {
        self.file_type.eq_ignore_ascii_case("D")
    }
    /// Whether this is a transaction-log file (`Type = 'L'`).
    pub fn is_log(&self) -> bool {
        self.file_type.eq_ignore_ascii_case("L")
    }
}

/// One physical file of an existing database, from `sys.master_files`. Used as
/// the relocation target when restoring over that database.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DbFile {
    pub logical_name: String,
    pub physical_name: String,
    /// `true` for the transaction log, `false` for a data file.
    pub is_log: bool,
}

/// A single `MOVE 'logical' TO 'to_path'` relocation for a restore.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileMove {
    pub logical_name: String,
    pub to_path: String,
}

/// Server default data/log directories (`SERVERPROPERTY('InstanceDefault…Path')`),
/// used as a fallback when a relocation target cannot be derived from the target
/// database's own files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DefaultDirs {
    pub data: String,
    pub log: String,
}

/// One entry when browsing a directory on the **server's** filesystem (via the
/// driver's directory lister). Names only — the caller tracks the absolute path
/// and joins with the server's separator.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ServerDirEntry {
    /// The entry's name (final path component), not a full path.
    pub name: String,
    /// `true` for a subdirectory, `false` for a file.
    pub is_dir: bool,
}

/// Split a server-side absolute file path into `(directory_with_trailing_sep,
/// file_name)`. Handles both Windows (`\`) and POSIX (`/`) paths by looking at
/// the last separator of either kind. The returned directory keeps its trailing
/// separator so [`join_server_path`] can re-use it.
fn split_dir(path: &str) -> (&str, &str) {
    match path.rfind(['\\', '/']) {
        Some(i) => (&path[..=i], &path[i + 1..]),
        None => ("", path),
    }
}

/// Join a server-side directory and a file name using the separator the
/// directory itself uses (so we match the *server's* OS, not the client's).
fn join_server_path(dir: &str, file: &str) -> String {
    let sep = if dir.contains('\\') { '\\' } else { '/' };
    if dir.ends_with(['\\', '/']) {
        format!("{dir}{file}")
    } else {
        format!("{dir}{sep}{file}")
    }
}

/// Plan the `MOVE` clauses for restoring `backup_files` (the contents of a
/// `.bak`) **over** an existing `target_files` database named `target_db`.
///
/// Strategy, per file class (data, then log), matched by position:
/// - **Reuse** the target database's existing physical path when the target has
///   a file at that position — a clean in-place overwrite, leaving no orphans.
/// - **Relocate** any extra backup file (the backup has more files of that class
///   than the target) into the same directory as the target's files, with a
///   deterministic name derived from `target_db`. The directory is taken from
///   the target's first file of that class; if the target has none, it falls
///   back to `default_dirs`.
///
/// Files that are neither data nor log (full-text `F`, filestream `S`) are
/// relocated into the data directory under a `target_db`-derived name so they
/// cannot collide with the source database's original paths.
pub fn plan_moves(
    backup_files: &[BackupFile],
    target_files: &[DbFile],
    default_dirs: &DefaultDirs,
    target_db: &str,
) -> Vec<FileMove> {
    let target_data: Vec<&DbFile> = target_files.iter().filter(|f| !f.is_log).collect();
    let target_log: Vec<&DbFile> = target_files.iter().filter(|f| f.is_log).collect();

    // Directory to drop *new* files of each class into (when the backup has more
    // files than the target): the target's own file directory if it has one,
    // else the server default.
    let data_dir = target_data
        .first()
        .map(|f| split_dir(&f.physical_name).0.to_string())
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| default_dirs.data.clone());
    let log_dir = target_log
        .first()
        .map(|f| split_dir(&f.physical_name).0.to_string())
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| default_dirs.log.clone());

    let mut moves = Vec::with_capacity(backup_files.len());
    let mut data_seen = 0usize;
    let mut log_seen = 0usize;

    for bf in backup_files {
        let to_path = if bf.is_log() {
            let i = log_seen;
            log_seen += 1;
            match target_log.get(i) {
                Some(t) => t.physical_name.clone(),
                None => {
                    let name = if i == 0 {
                        format!("{target_db}_log.ldf")
                    } else {
                        format!("{target_db}_log_{i}.ldf")
                    };
                    join_server_path(&log_dir, &name)
                }
            }
        } else if bf.is_data() {
            let i = data_seen;
            data_seen += 1;
            match target_data.get(i) {
                Some(t) => t.physical_name.clone(),
                None => {
                    let name = if i == 0 {
                        format!("{target_db}.mdf")
                    } else {
                        format!("{target_db}_{i}.ndf")
                    };
                    join_server_path(&data_dir, &name)
                }
            }
        } else {
            // Full-text / filestream / anything else: keep it off the source's
            // original path by placing it in the data dir under a derived name.
            join_server_path(&data_dir, &format!("{target_db}_{}", bf.logical_name))
        };
        moves.push(FileMove {
            logical_name: bf.logical_name.clone(),
            to_path,
        });
    }
    moves
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bf(logical: &str, physical: &str, ty: &str) -> BackupFile {
        BackupFile {
            logical_name: logical.into(),
            physical_name: physical.into(),
            file_type: ty.into(),
        }
    }
    fn df(logical: &str, physical: &str, is_log: bool) -> DbFile {
        DbFile {
            logical_name: logical.into(),
            physical_name: physical.into(),
            is_log,
        }
    }
    fn dirs() -> DefaultDirs {
        DefaultDirs {
            data: "/var/opt/mssql/data/".into(),
            log: "/var/opt/mssql/data/".into(),
        }
    }

    #[test]
    fn reuses_target_paths_when_layout_matches() {
        // Backup of "Source" (one data + one log) restored over "Target".
        let backup = vec![
            bf("Source", "/old/Source.mdf", "D"),
            bf("Source_log", "/old/Source_log.ldf", "L"),
        ];
        let target = vec![
            df("Target", "/srv/Target.mdf", false),
            df("Target_log", "/srv/Target_log.ldf", true),
        ];
        let moves = plan_moves(&backup, &target, &dirs(), "Target");
        assert_eq!(
            moves,
            vec![
                FileMove {
                    logical_name: "Source".into(),
                    to_path: "/srv/Target.mdf".into(),
                },
                FileMove {
                    logical_name: "Source_log".into(),
                    to_path: "/srv/Target_log.ldf".into(),
                },
            ]
        );
    }

    #[test]
    fn relocates_extra_backup_data_files_into_target_dir() {
        // Backup has two data files; target has one. The extra one lands in the
        // target's data directory under a derived name.
        let backup = vec![
            bf("d0", "/old/a.mdf", "D"),
            bf("d1", "/old/b.ndf", "D"),
            bf("lg", "/old/a.ldf", "L"),
        ];
        let target = vec![
            df("Tgt", "C:\\sql\\DATA\\Tgt.mdf", false),
            df("Tgt_log", "C:\\sql\\DATA\\Tgt_log.ldf", true),
        ];
        let moves = plan_moves(&backup, &target, &dirs(), "Tgt");
        assert_eq!(moves[0].to_path, "C:\\sql\\DATA\\Tgt.mdf"); // reuse
        assert_eq!(moves[1].to_path, "C:\\sql\\DATA\\Tgt_1.ndf"); // relocated, Windows sep
        assert_eq!(moves[2].to_path, "C:\\sql\\DATA\\Tgt_log.ldf"); // reuse log
    }

    #[test]
    fn falls_back_to_default_dirs_when_target_has_no_files() {
        let backup = vec![bf("d", "/old/d.mdf", "D"), bf("l", "/old/d.ldf", "L")];
        let target: Vec<DbFile> = vec![];
        let moves = plan_moves(&backup, &target, &dirs(), "New");
        assert_eq!(moves[0].to_path, "/var/opt/mssql/data/New.mdf");
        assert_eq!(moves[1].to_path, "/var/opt/mssql/data/New_log.ldf");
    }
}
