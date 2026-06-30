//! A heuristic SQL safety classifier that warns about destructive statements
//! *before* they run.
//!
//! **This is a heuristic, not a SQL parser.** It strips comments and string
//! literals, splits a batch into statements on top-level `;`, peels any leading
//! transaction-control keyword (`BEGIN TRAN`/`COMMIT`/`ROLLBACK`/…), and
//! inspects each statement's leading keyword. That is enough to flag the
//! dangerous cases a
//! desktop editor cares about (a `DELETE` with no `WHERE`, a `DROP`, anything
//! non-read on a read-only connection) without the weight — or the
//! dialect-specific edge cases — of a full grammar. It can misjudge exotic
//! constructs (CTEs that wrap a `DELETE`, vendor extensions, dynamic SQL inside
//! a string), and is intentionally conservative: when unsure it leans toward a
//! higher warning level, never a lower one.
//!
//! The verdict is advisory; the UI decides whether [`GuardLevel::Confirm`]
//! prompts the user and [`GuardLevel::Block`] refuses outright.

use serde::{Deserialize, Serialize};

/// How concerned the guard is about a statement (or batch).
///
/// `Ord` is derived with the variants in ascending severity, so
/// `Info < Confirm < Block` and `Iterator::max` yields the worst level across a
/// multi-statement batch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GuardLevel {
    /// Benign / read-only; no warning needed.
    Info,
    /// Potentially destructive; the UI should ask the user to confirm.
    Confirm,
    /// Disallowed for this connection (e.g. read-only mode); refuse to run.
    Block,
}

/// The classifier's decision for a batch of SQL.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuardVerdict {
    /// The highest severity found across all statements in the batch.
    pub level: GuardLevel,
    /// Human-readable reasons, in the order discovered. Empty for benign SQL.
    pub reasons: Vec<String>,
}

impl GuardVerdict {
    /// A benign, reason-free verdict.
    fn info() -> Self {
        Self {
            level: GuardLevel::Info,
            reasons: Vec::new(),
        }
    }
}

/// The category a statement's leading keyword puts it in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// SELECT / WITH / SHOW / EXPLAIN / PRINT / DECLARE / SET — read or benign.
    Read,
    /// INSERT / UPDATE / DELETE / MERGE — data modification.
    Dml,
    /// DROP / TRUNCATE / ALTER / CREATE / GRANT / REVOKE / EXEC[UTE] — schema
    /// changes or elevated operations.
    Ddl,
    /// A leading keyword we do not recognise. Treated as non-read (so it is
    /// blocked under read-only mode and flagged for confirmation), erring
    /// toward caution.
    Unknown,
}

impl Kind {
    /// Whether a statement of this kind reads only (no data/schema change).
    fn is_read(self) -> bool {
        matches!(self, Kind::Read)
    }
}

/// Classify a SQL batch for safety, honouring the connection's `read_only`
/// flag.
///
/// Returns the maximum [`GuardLevel`] across all statements together with the
/// collected reasons. Empty, whitespace-only, or comment-only input is
/// [`GuardLevel::Info`] with no reasons.
pub fn classify(sql: &str, read_only: bool) -> GuardVerdict {
    let sanitized = strip_comments_and_strings(sql);

    // Split on top-level `;` (string/comment `;` are already gone) and drop
    // empty fragments.
    let statements: Vec<&str> = sanitized
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();

    if statements.is_empty() {
        return GuardVerdict::info();
    }

    // A common SQL-editor safety pattern is:
    //   BEGIN TRAN; <try some DML>; ROLLBACK;
    // In writable mode, do not prompt for pure read/DML batches that are
    // explicitly rolled back. Keep read-only mode strict (the DML still runs and
    // takes locks/logs before rollback), and keep DDL/EXEC/unknown statements
    // cautious even inside a rollback wrapper.
    //
    // Two paths:
    // (a) Semicolon-separated: handled by is_rollback_wrapped_read_or_dml_batch.
    // (b) No semicolons (single "statement"): users often omit `;` in T-SQL.
    //     The token-level is_rollback_wrapped_no_semi covers that case.
    if !read_only
        && (is_rollback_wrapped_read_or_dml_batch(&statements)
            || (statements.len() == 1 && is_rollback_wrapped_no_semi(statements[0], false)))
    {
        return GuardVerdict::info();
    }

    let mut level = GuardLevel::Info;
    let mut reasons: Vec<String> = Vec::new();

    for stmt in statements {
        // Peel any leading transaction-control keywords (BEGIN TRAN, COMMIT,
        // ROLLBACK, SAVE TRAN, START TRANSACTION). They commit/abort a
        // transaction but write no data themselves, so the verdict should
        // reflect the statement (if any) that follows, not flag the control
        // keyword as "unrecognised".
        let body = match peel_leading_tcl(stmt) {
            Some(rest) => rest.trim(),
            None => stmt,
        };

        // A standalone transaction-control statement (nothing follows it) is
        // benign and allowed even on a read-only connection: it writes no data,
        // and any DML *inside* the transaction is judged on its own.
        if body.is_empty() {
            continue;
        }

        let kind = leading_kind(body);

        // Read-only mode is the strongest rule: any non-read statement is
        // blocked outright.
        if read_only && !kind.is_read() {
            level = level.max(GuardLevel::Block);
            push_unique(&mut reasons, "connection is in read-only mode");
            // Still fall through so we also record *why* it was dangerous,
            // which is useful context in the UI alongside the block.
        }

        match kind {
            Kind::Read => {}
            Kind::Dml => classify_dml(body, &mut level, &mut reasons),
            Kind::Ddl => classify_ddl(body, &mut level, &mut reasons),
            Kind::Unknown => {
                // Unknown leading keyword: confirm, but do not block (unless
                // read-only already blocked it above).
                level = level.max(GuardLevel::Confirm);
                push_unique(
                    &mut reasons,
                    "unrecognised statement; review before running",
                );
            }
        }
    }

    GuardVerdict { level, reasons }
}

/// Whether `sql` is a batch the MSSQL driver can run via tiberius' `execute()`
/// to obtain per-statement **affected-row counts** — which the streaming
/// `simple_query` path cannot surface (tiberius' `QueryStream` silently drops
/// the TDS DONE token that carries the count).
///
/// True in two cases:
///
/// 1. **DML + variable-ops batch**: every statement is either a
///    data-modification (`INSERT` / `UPDATE` / `DELETE` / `MERGE`) with **no
///    `OUTPUT` clause**, or a non-row-returning variable operation (`DECLARE` /
///    `SET`). At least one DML statement must be present — a variable-ops-only
///    batch has nothing to count.
///
///    `DECLARE` and `SET` are safe on the `execute()` path because SQL Server
///    does not set the TDS `COUNT` flag on their DONE tokens, so tiberius
///    does not include them in `rows_affected()` — the array naturally contains
///    only the DML counts.
///
/// 2. **Rollback-wrapped DML**: the batch is `BEGIN TRAN[SACTION]; <DML /
///    variable-ops …>; ROLLBACK` (with or without semicolons). SQL Server
///    reports the affected-row DONE token for each inner DML even though the
///    transaction is rolled back, giving the user a safe dry-run row count.
///    `execute()` collects those counts correctly; the inner DML must not have
///    an `OUTPUT` clause (which would return rows that `execute()` silently
///    discards), and must not contain `SELECT` / `WITH` for the same reason.
///
/// Everything else — `SELECT` / `EXEC` / DDL / `USE` / CTEs / committed
/// transaction wrappers / batches with SELECT — stays on the row-streaming
/// path. Reuses the same comment/string stripping as [`classify`], so a
/// `DELETE` or `OUTPUT` inside a string literal cannot mislead it.
pub(crate) fn is_countable_dml_batch(sql: &str) -> bool {
    let sanitized = strip_comments_and_strings(sql);
    let stmts: Vec<&str> = sanitized
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();

    if stmts.is_empty() {
        return false;
    }

    // Rollback-wrapped DML is countable (semicolon-separated or semicolon-free).
    if is_rollback_wrapped_dml_only_batch(&stmts)
        || (stmts.len() == 1 && is_rollback_wrapped_no_semi(stmts[0], true))
    {
        return true;
    }

    // "DML + variable-ops" batch: every statement must be either DML without an
    // OUTPUT clause, or a non-row-returning variable operation (DECLARE / SET).
    // At least one DML is required — a variable-ops-only batch has nothing to count.
    //
    // DECLARE and SET are safe on the execute() path: SQL Server does not set
    // the TDS COUNT flag in their DONE tokens, so tiberius does not include them
    // in rows_affected() — the array naturally contains only the DML counts.
    let mut has_dml = false;
    for stmt in &stmts {
        match leading_kind(stmt) {
            Kind::Dml => {
                if contains_keyword(stmt, "OUTPUT") {
                    return false;
                }
                has_dml = true;
            }
            Kind::Read => {
                // Only DECLARE and SET are allowed as non-DML statements.
                // SELECT, WITH, USE, etc. would return rows that execute() discards.
                let kw_upper = first_keyword(stmt)
                    .map(|k| k.to_ascii_uppercase())
                    .unwrap_or_default();
                if !matches!(kw_upper.as_str(), "DECLARE" | "SET") {
                    return false;
                }
            }
            _ => return false,
        }
    }
    has_dml
}

/// Whether `sql` is specifically a **rollback-wrapped** countable-DML batch
/// (the dry-run pattern `BEGIN TRAN[SACTION]; <DML …>; ROLLBACK`, with or
/// without semicolons).
///
/// The MSSQL driver uses this — after [`is_countable_dml_batch`] has already
/// gated the batch onto the counting path — to know it must **trim the leading
/// and trailing zero-count entries** emitted by the `BEGIN TRAN` and `ROLLBACK`
/// statements, so the UI shows only the inner DML's "<n> rows affected" set
/// instead of two phantom 0-row sets framing the real one.
pub(crate) fn is_rollback_wrapped_dml_batch(sql: &str) -> bool {
    let sanitized = strip_comments_and_strings(sql);
    let stmts: Vec<&str> = sanitized
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();

    if stmts.is_empty() {
        return false;
    }
    is_rollback_wrapped_dml_only_batch(&stmts)
        || (stmts.len() == 1 && is_rollback_wrapped_no_semi(stmts[0], true))
}

/// Peel one or more leading `USE <database>` statements off the front of a
/// batch.
///
/// Returns `Some((use_statements, remainder))` when the batch begins with at
/// least one top-level, `;`-terminated `USE …` statement: `use_statements`
/// holds the original text of each (in order, including any leading comments)
/// and `remainder` is the rest of the batch after them (possibly only
/// whitespace). Returns `None` when the batch does not start with such a `USE`.
///
/// Comments and string literals are skipped when locating statement boundaries
/// and the leading keyword, so a `;` or the word `USE` inside a literal/comment
/// cannot mislead it. It scans the original `sql` (not the sanitized copy) so
/// `remainder` preserves the user's exact text for execution.
///
/// The MSSQL driver uses this to run context-changing `USE` statements on the
/// persistent batch path — so the connection's current database actually
/// changes, which the `sp_executesql`-based affected-count path would *not* do
/// — while still routing the remaining DML through the counting path. A `USE`
/// without a trailing `;` is intentionally left in place (handled by the normal
/// dispatch), so only clearly-delimited leading `USE`s are peeled.
pub(crate) fn peel_leading_use(sql: &str) -> Option<(Vec<&str>, &str)> {
    let bytes = sql.as_bytes();
    let mut uses: Vec<&str> = Vec::new();
    // Byte offset where the current statement begins in the original `sql`.
    let mut stmt_start = 0usize;
    let mut i = 0usize;

    while i < bytes.len() {
        let b = bytes[i];

        // Skip a line comment to end of line.
        if b == b'-' && i + 1 < bytes.len() && bytes[i + 1] == b'-' {
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        // Skip a block comment.
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
            continue;
        }
        // Skip a single-quoted string literal (with `''` escaping).
        if b == b'\'' {
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\'' {
                    if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }

        if b == b';' {
            let stmt = &sql[stmt_start..i];
            if statement_starts_with_use(stmt) {
                uses.push(stmt);
                i += 1;
                stmt_start = i;
                continue;
            }
            // First non-USE statement reached; the remainder starts here.
            break;
        }
        i += 1;
    }

    if uses.is_empty() {
        return None;
    }
    Some((uses, &sql[stmt_start..]))
}

/// Whether `stmt`'s first SQL keyword (comments/strings stripped) is `USE`.
fn statement_starts_with_use(stmt: &str) -> bool {
    let sanitized = strip_comments_and_strings(stmt);
    first_keyword(&sanitized).is_some_and(|kw| kw.eq_ignore_ascii_case("USE"))
}

/// Whether the whole batch is an explicit transaction that ends in `ROLLBACK`
/// and contains only read/DML statements inside it. Used to allow the common
/// "dry-run an UPDATE then rollback" workflow without a destructive-SQL modal.
fn is_rollback_wrapped_read_or_dml_batch(statements: &[&str]) -> bool {
    if statements.len() < 2 {
        return false;
    }

    let last = statements[statements.len() - 1];
    if !is_rollback_statement(last) {
        return false;
    }
    // The first statement must open a transaction. Peel the opener; any inline
    // remainder (no `;` after `BEGIN TRAN`) becomes the first inner statement.
    let Some(first_inner) = peel_transaction_opener(statements[0]) else {
        return false;
    };

    for stmt in inner_statements(first_inner, statements) {
        if is_commit_statement(stmt) {
            return false;
        }

        let body = match peel_leading_tcl(stmt) {
            Some(rest) => rest.trim(),
            None => stmt.trim(),
        };
        if body.is_empty() {
            continue;
        }

        if !matches!(leading_kind(body), Kind::Read | Kind::Dml) {
            return false;
        }
    }

    true
}

/// The inner statements of a rollback-wrapped batch: the inline remainder of
/// the opening statement (when `BEGIN TRAN` had no `;` after it), followed by
/// the statements between the opener and the trailing `ROLLBACK`. Empty
/// fragments are skipped by callers via `body.is_empty()`.
fn inner_statements<'a>(
    first_inner: &'a str,
    statements: &'a [&'a str],
) -> impl Iterator<Item = &'a str> {
    std::iter::once(first_inner.trim())
        .filter(|s| !s.is_empty())
        .chain(statements[1..statements.len() - 1].iter().copied())
}

/// If `stmt` begins with a transaction opener (`BEGIN TRAN[SACTION]` or
/// `START TRANSACTION`), return the text *after* the opener: empty when the
/// opener stands alone (`BEGIN TRAN`), or the inline first inner statement when
/// the user wrote no `;` after it (`BEGIN TRANSACTION\nDECLARE @x INT`).
/// Returns `None` when there is no leading opener.
///
/// Peeling the opener — rather than requiring it to stand alone in its own
/// `;`-delimited statement — lets the rollback-wrapper detectors recognise the
/// very common T-SQL style where `BEGIN TRANSACTION` sits on its own line with
/// no terminating semicolon, so the `;`-split glues it to the following
/// statement.
fn peel_transaction_opener(stmt: &str) -> Option<&str> {
    let (kw, rest) = split_first_word(stmt)?;
    let is = |w: &str, target: &str| w.eq_ignore_ascii_case(target);
    match kw.to_ascii_uppercase().as_str() {
        "BEGIN" => match split_first_word(rest) {
            Some((w, after)) if is(w, "TRAN") || is(w, "TRANSACTION") => Some(after),
            _ => None,
        },
        "START" => match split_first_word(rest) {
            Some((w, after)) if is(w, "TRANSACTION") => Some(after),
            _ => None,
        },
        _ => None,
    }
}

fn is_rollback_statement(stmt: &str) -> bool {
    let Some((kw, rest)) = split_first_word(stmt) else {
        return false;
    };
    if !kw.eq_ignore_ascii_case("ROLLBACK") {
        return false;
    }
    let rest = rest.trim();
    if rest.is_empty() {
        return true;
    }
    match split_first_word(rest) {
        Some((w, after))
            if (w.eq_ignore_ascii_case("TRAN") || w.eq_ignore_ascii_case("TRANSACTION")) =>
        {
            after.trim().is_empty()
        }
        Some((w, after)) if w.eq_ignore_ascii_case("WORK") => after.trim().is_empty(),
        _ => false,
    }
}

fn is_commit_statement(stmt: &str) -> bool {
    first_keyword(stmt).is_some_and(|kw| kw.eq_ignore_ascii_case("COMMIT"))
}

/// Token-level check for the no-semicolon rollback-wrapped batch pattern.
///
/// Users often write T-SQL without semicolons between statements:
/// ```sql
/// BEGIN TRANSACTION
/// UPDATE t SET x = 1 WHERE id = 1
/// ROLLBACK
/// ```
/// The normal `;`-split in [`classify`] / [`is_countable_dml_batch`] treats
/// this as a single statement and misses the rollback wrapper. This function
/// re-splits the already-stripped text on whitespace and checks:
///
/// - First two tokens: `BEGIN TRAN[SACTION]`
/// - Last one or two tokens: `ROLLBACK [TRAN | TRANSACTION | WORK]`
/// - Middle tokens: no DDL (`DROP`, `TRUNCATE`, `CREATE`, `ALTER`, `GRANT`,
///   `REVOKE`, `EXEC[UTE]`), no `COMMIT`, no `OUTPUT`.
///
/// When `dml_only` is `true` (used by [`is_countable_dml_batch`]): the middle
/// must also contain at least one explicit DML verb (`INSERT`, `UPDATE`,
/// `DELETE`, `MERGE`) and must not contain `SELECT` or `WITH` (which would
/// return rows that `execute()` silently discards).
///
/// When `dml_only` is `false` (used by the guard): reads are also accepted,
/// consistent with the semicolon-based [`is_rollback_wrapped_read_or_dml_batch`].
fn is_rollback_wrapped_no_semi(sanitized: &str, dml_only: bool) -> bool {
    let words: Vec<&str> = sanitized.split_ascii_whitespace().collect();
    // Minimum: BEGIN TRAN <one middle token> ROLLBACK (4 tokens).
    if words.len() < 4 {
        return false;
    }

    // --- Opener: BEGIN TRAN[SACTION] ---
    let opener_len = match words[0].to_ascii_uppercase().as_str() {
        "BEGIN" => match words[1].to_ascii_uppercase().as_str() {
            "TRAN" | "TRANSACTION" => 2,
            _ => return false,
        },
        _ => return false,
    };

    // --- Closer: ROLLBACK [TRAN | TRANSACTION | WORK] (1 or 2 trailing tokens) ---
    let n = words.len();
    let closer_start = if words[n - 1].eq_ignore_ascii_case("ROLLBACK") {
        n - 1
    } else if n >= 3 && words[n - 2].eq_ignore_ascii_case("ROLLBACK") {
        let tail = words[n - 1].to_ascii_uppercase();
        if matches!(tail.as_str(), "TRAN" | "TRANSACTION" | "WORK") {
            n - 2
        } else {
            return false;
        }
    } else {
        return false;
    };

    // Middle must be non-empty.
    if closer_start <= opener_len {
        return false;
    }

    let middle = &words[opener_len..closer_start];
    let mut saw_dml = false;

    for word in middle {
        let upper = word.to_ascii_uppercase();
        match upper.as_str() {
            "INSERT" | "UPDATE" | "DELETE" | "MERGE" => saw_dml = true,
            // DDL / elevated ops — reject even inside a rollback, same as
            // the semicolon-based check.
            "DROP" | "TRUNCATE" | "CREATE" | "ALTER" | "GRANT" | "REVOKE" | "EXEC" | "EXECUTE"
            | "OUTPUT" => return false,
            // COMMIT inside the batch means data is persisted — reject.
            "COMMIT" => return false,
            // SELECTs / CTEs return rows that client.execute() would discard.
            "SELECT" | "WITH" if dml_only => return false,
            _ => {}
        }
    }

    // Countable-DML path: at least one explicit DML verb required.
    if dml_only && !saw_dml {
        return false;
    }

    true
}

/// Whether a semicolon-separated batch is a rollback-wrapped pure-DML batch:
/// opens with a standalone transaction opener, ends with `ROLLBACK`, and every
/// statement in between is DML without an `OUTPUT` clause.
///
/// Used by [`is_countable_dml_batch`] so that a `BEGIN TRAN; UPDATE …; ROLLBACK`
/// dry-run is routed through `execute()` and reports the affected row count.
fn is_rollback_wrapped_dml_only_batch(statements: &[&str]) -> bool {
    if statements.len() < 2 {
        return false;
    }
    let last = statements[statements.len() - 1];
    if !is_rollback_statement(last) {
        return false;
    }
    let Some(first_inner) = peel_transaction_opener(statements[0]) else {
        return false;
    };

    let mut saw_dml = false;
    for stmt in inner_statements(first_inner, statements) {
        if is_commit_statement(stmt) {
            return false;
        }
        let body = match peel_leading_tcl(stmt) {
            Some(rest) => rest.trim(),
            None => stmt.trim(),
        };
        if body.is_empty() {
            continue;
        }
        match leading_kind(body) {
            Kind::Dml => {
                if contains_keyword(body, "OUTPUT") {
                    return false;
                }
                saw_dml = true;
            }
            Kind::Read => {
                // DECLARE and SET are allowed: they never return result sets.
                let kw_upper = first_keyword(body)
                    .map(|k| k.to_ascii_uppercase())
                    .unwrap_or_default();
                if !matches!(kw_upper.as_str(), "DECLARE" | "SET") {
                    return false;
                }
            }
            _ => return false,
        }
    }

    // Must have at least one inner DML — a bare BEGIN TRAN; ROLLBACK is not
    // "countable" (nothing to count).
    saw_dml
}

/// Classify a DML statement (INSERT/UPDATE/DELETE/MERGE).
fn classify_dml(stmt: &str, level: &mut GuardLevel, reasons: &mut Vec<String>) {
    let kw = first_keyword(stmt).unwrap_or_default();
    let is_update_or_delete =
        kw.eq_ignore_ascii_case("UPDATE") || kw.eq_ignore_ascii_case("DELETE");

    // The headline risk: a mass UPDATE/DELETE with no WHERE touches every row.
    if is_update_or_delete && !has_where_clause(stmt) {
        bump(level, GuardLevel::Confirm);
        push_unique(reasons, "UPDATE/DELETE without WHERE affects all rows");
        return;
    }

    // Otherwise still a write worth confirming, but a milder reason.
    bump(level, GuardLevel::Confirm);
    push_unique(reasons, &format!("{} modifies data", kw.to_uppercase()));
}

/// Classify a DDL / elevated statement.
fn classify_ddl(stmt: &str, level: &mut GuardLevel, reasons: &mut Vec<String>) {
    let kw = first_keyword(stmt).unwrap_or_default().to_uppercase();
    bump(level, GuardLevel::Confirm);

    // TRUNCATE and DROP are irreversible bulk operations; name them explicitly.
    match kw.as_str() {
        "TRUNCATE" => push_unique(reasons, "TRUNCATE removes all rows"),
        "DROP" => push_unique(reasons, "DROP permanently removes a database object"),
        "RESTORE" => push_unique(reasons, "RESTORE overwrites the database from a backup"),
        "BACKUP" => push_unique(reasons, "BACKUP writes the database to a server-side file"),
        other => push_unique(reasons, &format!("{other} is a schema/elevated operation")),
    }
}

/// Determine the [`Kind`] of a statement from its first keyword.
fn leading_kind(stmt: &str) -> Kind {
    let Some(kw) = first_keyword(stmt) else {
        return Kind::Unknown;
    };
    // Compare case-insensitively against the known keyword sets.
    let upper = kw.to_uppercase();
    match upper.as_str() {
        "SELECT" | "WITH" | "SHOW" | "EXPLAIN" | "PRINT" | "DECLARE" | "SET" | "USE" => Kind::Read,
        "INSERT" | "UPDATE" | "DELETE" | "MERGE" => Kind::Dml,
        "DROP" | "TRUNCATE" | "ALTER" | "CREATE" | "GRANT" | "REVOKE" | "EXEC" | "EXECUTE"
        | "BACKUP" | "RESTORE" => Kind::Ddl,
        _ => Kind::Unknown,
    }
}

/// Split off the leading SQL word (a run of ASCII alphabetic characters) of
/// `s`, returning `(word, rest)` where `rest` is everything after the word.
/// Leading whitespace is skipped. Returns `None` when there is no leading
/// letter (empty input, or text starting with punctuation such as a `(`).
fn split_first_word(s: &str) -> Option<(&str, &str)> {
    let trimmed = s.trim_start();
    let end = trimmed
        .char_indices()
        .find(|(_, c)| !c.is_ascii_alphabetic())
        .map(|(i, _)| i)
        .unwrap_or(trimmed.len());
    if end == 0 {
        None
    } else {
        Some((&trimmed[..end], &trimmed[end..]))
    }
}

/// Extract the first word (leading SQL keyword) of a statement, if any.
fn first_keyword(stmt: &str) -> Option<&str> {
    split_first_word(stmt).map(|(word, _)| word)
}

/// If `stmt` begins with a transaction-control phrase, return the remainder
/// after that phrase (empty for a standalone `COMMIT` / `ROLLBACK` /
/// `BEGIN TRANSACTION`). Returns `None` when there is no leading TCL keyword.
///
/// Recognised case-insensitively: `BEGIN TRAN[SACTION]`, `START TRANSACTION`,
/// `COMMIT [WORK | TRAN[SACTION] …]`, `ROLLBACK [WORK | TRAN[SACTION] …]`,
/// `SAVE TRAN[SACTION] …`. Transaction control writes no data of its own, so
/// the verdict should reflect the statement (if any) that follows — not flag
/// the control keyword itself as "unrecognised".
///
/// A *bare* `BEGIN` (a `BEGIN…END` block, or `BEGIN TRY`) is deliberately NOT
/// peeled: its body can contain anything, so it stays subject to the normal,
/// cautious rules rather than being silently downgraded to benign.
fn peel_leading_tcl(stmt: &str) -> Option<&str> {
    let (kw, rest) = split_first_word(stmt)?;
    let is = |w: &str, target: &str| w.eq_ignore_ascii_case(target);
    match kw.to_ascii_uppercase().as_str() {
        // Standalone control. Nothing dangerous can legitimately follow on the
        // same `;`-delimited statement, so the remainder is consumed.
        "COMMIT" | "ROLLBACK" => Some(""),
        "SAVE" => match split_first_word(rest) {
            Some((w, _)) if is(w, "TRAN") || is(w, "TRANSACTION") => Some(""),
            _ => None,
        },
        // Openers: peel the opener but keep the remainder, so an inline
        // statement (e.g. a no-semicolon `BEGIN TRAN UPDATE …`) is still
        // classified rather than hidden behind the opener.
        "BEGIN" => match split_first_word(rest) {
            Some((w, after)) if is(w, "TRAN") || is(w, "TRANSACTION") => Some(after),
            _ => None,
        },
        "START" => match split_first_word(rest) {
            Some((w, after)) if is(w, "TRANSACTION") => Some(after),
            _ => None,
        },
        _ => None,
    }
}

/// Whether `stmt` contains a `WHERE` clause, matched as a whole word,
/// case-insensitively. Operates on already-sanitized text (no strings/comments),
/// so a literal `'where'` cannot trigger a false positive.
fn has_where_clause(stmt: &str) -> bool {
    contains_keyword(stmt, "WHERE")
}

/// Word-boundary, case-insensitive search for `keyword` within `haystack`.
///
/// Avoids matching substrings inside identifiers (e.g. `WHERE` must not match
/// `nowhere` or `where_id`). Boundaries are non-alphanumeric, non-underscore
/// characters.
fn contains_keyword(haystack: &str, keyword: &str) -> bool {
    let hay = haystack.as_bytes();
    let needle = keyword.as_bytes();
    if needle.is_empty() || hay.len() < needle.len() {
        return false;
    }

    let is_word_byte = |b: u8| b.is_ascii_alphanumeric() || b == b'_';

    let mut i = 0;
    while i + needle.len() <= hay.len() {
        let window = &hay[i..i + needle.len()];
        if window.eq_ignore_ascii_case(needle) {
            let before_ok = i == 0 || !is_word_byte(hay[i - 1]);
            let after_idx = i + needle.len();
            let after_ok = after_idx >= hay.len() || !is_word_byte(hay[after_idx]);
            if before_ok && after_ok {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// Remove SQL comments and string-literal *contents* so keyword detection and
/// statement splitting see only structural SQL.
///
/// Handled:
/// - `-- line comments` to end of line
/// - `/* block comments */` (non-nested — the common case)
/// - `'single-quoted strings'` with `''` escaping, replaced by an empty `''`
///   so a `;` or the word `delete` inside a literal cannot mislead the scanner.
///
/// Double-quoted / bracketed identifiers are left intact: they are names, and
/// keeping them does not cause false destructive matches.
fn strip_comments_and_strings(sql: &str) -> String {
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len());
    // Start of the current run of ordinary (copy-through) bytes. We copy such
    // runs as whole UTF-8 string slices so multi-byte characters survive
    // intact — scanning the delimiters below is ASCII-only and safe on bytes,
    // but content must be emitted as proper `str` slices, never `byte as char`.
    let mut run_start = 0;
    let mut i = 0;

    // Flush the pending ordinary run [run_start, end) to the output.
    macro_rules! flush_run {
        ($end:expr) => {
            if run_start < $end {
                out.push_str(&sql[run_start..$end]);
            }
        };
    }

    while i < bytes.len() {
        let b = bytes[i];

        // Line comment: -- … to newline.
        if b == b'-' && i + 1 < bytes.len() && bytes[i + 1] == b'-' {
            flush_run!(i);
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            // Leave i at the newline (if any); it becomes the next run's start
            // and is copied through as whitespace, preserving token boundaries.
            run_start = i;
            continue;
        }

        // Block comment: /* … */ (non-nested).
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            flush_run!(i);
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            // Skip the closing */ if present.
            i = (i + 2).min(bytes.len());
            // Emit a space so `a/* x */b` does not fuse into `ab`.
            out.push(' ');
            run_start = i;
            continue;
        }

        // Single-quoted string literal. Replace its contents with nothing,
        // collapsing to an empty literal `''` in the output.
        if b == b'\'' {
            flush_run!(i);
            i += 1; // consume opening quote
            while i < bytes.len() {
                if bytes[i] == b'\'' {
                    // A doubled '' is an escaped quote *inside* the string, not
                    // a terminator: skip both and keep scanning.
                    if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                        i += 2;
                        continue;
                    }
                    i += 1; // consume closing quote
                    break;
                }
                i += 1;
            }
            // Emit an empty string literal placeholder so surrounding syntax
            // (e.g. `VALUES ('')`) stays well-formed for the heuristics.
            out.push_str("''");
            run_start = i;
            continue;
        }

        // Ordinary byte: extend the current run. Delimiter bytes above are all
        // ASCII, so a UTF-8 continuation byte (>= 0x80) can never be mistaken
        // for one; advancing by a single byte keeps run boundaries on char
        // boundaries because we only ever cut at the ASCII delimiters.
        i += 1;
    }

    // Flush the trailing ordinary run.
    flush_run!(bytes.len());
    out
}

/// Raise `level` in place to at least `floor` (never lowers it).
fn bump(level: &mut GuardLevel, floor: GuardLevel) {
    *level = (*level).max(floor);
}

/// Push `reason` only if not already present, keeping the reason list concise
/// when a batch repeats the same kind of statement.
fn push_unique(reasons: &mut Vec<String>, reason: &str) {
    if !reasons.iter().any(|r| r == reason) {
        reasons.push(reason.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify_rw(sql: &str) -> GuardVerdict {
        classify(sql, false)
    }

    #[test]
    fn plain_select_is_info() {
        let v = classify_rw("SELECT * FROM users");
        assert_eq!(v.level, GuardLevel::Info);
        assert!(v.reasons.is_empty());
    }

    #[test]
    fn select_case_insensitive_and_cte_with() {
        assert_eq!(classify_rw("select 1").level, GuardLevel::Info);
        assert_eq!(
            classify_rw("WITH c AS (SELECT 1) SELECT * FROM c").level,
            GuardLevel::Info
        );
        assert_eq!(classify_rw("EXPLAIN SELECT 1").level, GuardLevel::Info);
    }

    #[test]
    fn delete_without_where_confirms_with_all_rows_reason() {
        let v = classify_rw("DELETE FROM t");
        assert_eq!(v.level, GuardLevel::Confirm);
        assert!(v
            .reasons
            .iter()
            .any(|r| r.contains("without WHERE affects all rows")));
    }

    #[test]
    fn update_without_where_confirms() {
        let v = classify_rw("UPDATE t SET x = 1");
        assert_eq!(v.level, GuardLevel::Confirm);
        assert!(v
            .reasons
            .iter()
            .any(|r| r.contains("without WHERE affects all rows")));
    }

    #[test]
    fn delete_with_where_confirms_but_milder_reason() {
        let v = classify_rw("DELETE FROM t WHERE id = 1");
        assert_eq!(v.level, GuardLevel::Confirm);
        // Not the "all rows" reason — the milder "modifies data" one.
        assert!(v.reasons.iter().any(|r| r.contains("modifies data")));
        assert!(!v
            .reasons
            .iter()
            .any(|r| r.contains("without WHERE affects all rows")));
    }

    #[test]
    fn insert_confirms() {
        let v = classify_rw("INSERT INTO t (a) VALUES (1)");
        assert_eq!(v.level, GuardLevel::Confirm);
        assert!(v.reasons.iter().any(|r| r.contains("INSERT modifies data")));
    }

    #[test]
    fn drop_table_confirms_and_names_it() {
        let v = classify_rw("DROP TABLE t");
        assert_eq!(v.level, GuardLevel::Confirm);
        assert!(v.reasons.iter().any(|r| r.contains("DROP")));
    }

    #[test]
    fn truncate_confirms_and_names_it() {
        let v = classify_rw("TRUNCATE TABLE t");
        assert_eq!(v.level, GuardLevel::Confirm);
        assert!(v.reasons.iter().any(|r| r.contains("TRUNCATE")));
    }

    #[test]
    fn alter_and_create_are_confirm() {
        assert_eq!(
            classify_rw("ALTER TABLE t ADD c INT").level,
            GuardLevel::Confirm
        );
        assert_eq!(
            classify_rw("CREATE TABLE t (id INT)").level,
            GuardLevel::Confirm
        );
    }

    #[test]
    fn multi_statement_takes_max_severity() {
        // A benign SELECT followed by a DROP → overall Confirm, both reasons.
        let v = classify_rw("SELECT 1; DROP TABLE t");
        assert_eq!(v.level, GuardLevel::Confirm);
        assert!(v.reasons.iter().any(|r| r.contains("DROP")));
    }

    #[test]
    fn read_only_blocks_dml() {
        let v = classify("DELETE FROM t WHERE id = 1", true);
        assert_eq!(v.level, GuardLevel::Block);
        assert!(v.reasons.iter().any(|r| r.contains("read-only mode")));
    }

    #[test]
    fn read_only_allows_select() {
        let v = classify("SELECT * FROM t", true);
        assert_eq!(v.level, GuardLevel::Info);
        assert!(v.reasons.is_empty());
    }

    #[test]
    fn read_only_blocks_ddl_at_block_level() {
        let v = classify("DROP TABLE t", true);
        assert_eq!(v.level, GuardLevel::Block);
        // Both the read-only reason and the DROP context are recorded.
        assert!(v.reasons.iter().any(|r| r.contains("read-only mode")));
        assert!(v.reasons.iter().any(|r| r.contains("DROP")));
    }

    #[test]
    fn semicolon_inside_string_does_not_split() {
        // The `;` and the word DELETE live inside a literal, so this is one
        // benign INSERT, not an INSERT + a DELETE.
        let v = classify_rw("INSERT INTO t (note) VALUES ('a; DELETE FROM x')");
        assert_eq!(v.level, GuardLevel::Confirm);
        // Only the INSERT reason; the literal must not introduce a DELETE.
        assert!(v.reasons.iter().any(|r| r.contains("INSERT modifies data")));
        assert!(!v
            .reasons
            .iter()
            .any(|r| r.contains("without WHERE affects all rows")));
    }

    #[test]
    fn word_delete_in_string_literal_is_ignored_for_where_check() {
        // A DELETE whose WHERE value contains the word "where"/"delete" must
        // still be detected as having a WHERE (milder reason), not mass-delete.
        let v = classify_rw("DELETE FROM t WHERE note = 'please delete everything'");
        assert_eq!(v.level, GuardLevel::Confirm);
        assert!(v.reasons.iter().any(|r| r.contains("modifies data")));
        assert!(!v
            .reasons
            .iter()
            .any(|r| r.contains("without WHERE affects all rows")));
    }

    #[test]
    fn line_comment_with_delete_is_ignored() {
        let v = classify_rw("SELECT 1 -- DELETE FROM t");
        assert_eq!(v.level, GuardLevel::Info);
        assert!(v.reasons.is_empty());
    }

    #[test]
    fn block_comment_with_drop_is_ignored() {
        let v = classify_rw("SELECT 1 /* DROP TABLE t */ FROM dual");
        assert_eq!(v.level, GuardLevel::Info);
    }

    #[test]
    fn comment_only_input_is_info() {
        assert_eq!(classify_rw("-- just a note").level, GuardLevel::Info);
        assert_eq!(classify_rw("/* nothing here */").level, GuardLevel::Info);
    }

    #[test]
    fn empty_and_whitespace_are_info() {
        assert_eq!(classify_rw("").level, GuardLevel::Info);
        assert!(classify_rw("").reasons.is_empty());
        assert_eq!(classify_rw("   \n\t ").level, GuardLevel::Info);
        assert_eq!(classify_rw(";;;").level, GuardLevel::Info);
    }

    #[test]
    fn trailing_semicolon_does_not_create_empty_statement() {
        let v = classify_rw("SELECT 1;");
        assert_eq!(v.level, GuardLevel::Info);
        assert!(v.reasons.is_empty());
    }

    #[test]
    fn where_must_be_whole_word() {
        // An UPDATE on a column literally named via an identifier containing
        // "where" must NOT be read as having a WHERE clause.
        let v = classify_rw("UPDATE t SET nowhere = 1");
        assert_eq!(v.level, GuardLevel::Confirm);
        assert!(v
            .reasons
            .iter()
            .any(|r| r.contains("without WHERE affects all rows")));
    }

    #[test]
    fn guard_level_ordering_holds() {
        assert!(GuardLevel::Info < GuardLevel::Confirm);
        assert!(GuardLevel::Confirm < GuardLevel::Block);
        assert_eq!(
            [GuardLevel::Info, GuardLevel::Block, GuardLevel::Confirm]
                .into_iter()
                .max(),
            Some(GuardLevel::Block)
        );
    }

    #[test]
    fn use_database_is_info_on_writable_and_read_only() {
        // USE only changes session context; it must not prompt a confirm dialog.
        assert_eq!(classify_rw("USE mydb").level, GuardLevel::Info);
        assert_eq!(classify_rw("USE [my-db]").level, GuardLevel::Info);
        // Even on a read-only connection USE is allowed — no data is written.
        assert_eq!(classify("USE mydb", true).level, GuardLevel::Info);
    }

    #[test]
    fn unknown_keyword_confirms_but_does_not_block_when_writable() {
        let v = classify_rw("VACUUM");
        assert_eq!(v.level, GuardLevel::Confirm);
        // And it blocks under read-only.
        assert_eq!(classify("VACUUM", true).level, GuardLevel::Block);
    }

    #[test]
    fn reasons_are_deduplicated_across_statements() {
        let v = classify_rw("INSERT INTO t VALUES (1); INSERT INTO t VALUES (2)");
        assert_eq!(v.level, GuardLevel::Confirm);
        let insert_reasons = v
            .reasons
            .iter()
            .filter(|r| r.contains("INSERT modifies data"))
            .count();
        assert_eq!(insert_reasons, 1, "duplicate reasons should be collapsed");
    }

    #[test]
    fn verdict_serde_roundtrips() {
        let v = GuardVerdict {
            level: GuardLevel::Confirm,
            reasons: vec!["x".into()],
        };
        let json = serde_json::to_string(&v).unwrap();
        assert!(json.contains("\"confirm\""));
        let back: GuardVerdict = serde_json::from_str(&json).unwrap();
        assert_eq!(back, v);
    }

    #[test]
    fn multibyte_unicode_is_preserved_by_the_stripper() {
        // Multi-byte UTF-8 outside string literals must pass through intact
        // (no `byte as char` mojibake) and not panic on a non-char-boundary
        // slice. An accented identifier survives; the SELECT stays Info.
        let stripped = strip_comments_and_strings("SELECT café FROM tüsch -- côté");
        assert!(stripped.contains("café"), "got: {stripped:?}");
        assert!(stripped.contains("tüsch"), "got: {stripped:?}");
        // The line comment (with its multi-byte content) is gone.
        assert!(!stripped.contains("côté"), "got: {stripped:?}");

        // And classification is unaffected by the non-ASCII content.
        let v = classify_rw("SELECT café FROM tüsch");
        assert_eq!(v.level, GuardLevel::Info);
        assert!(v.reasons.is_empty());
    }

    #[test]
    fn multibyte_unicode_inside_string_literal_does_not_panic() {
        // A multi-byte char inside a single-quoted literal is scanned byte-wise
        // but the literal is replaced wholesale; the surrounding statement must
        // classify correctly and slicing must stay on char boundaries.
        let v = classify_rw("INSERT INTO t (note) VALUES ('héllo; DROP TABLE x')");
        assert_eq!(v.level, GuardLevel::Confirm);
        assert!(v.reasons.iter().any(|r| r.contains("INSERT modifies data")));
        // The DROP inside the literal must not leak out.
        assert!(!v.reasons.iter().any(|r| r.contains("DROP")));
    }

    // --- transaction control (TCL) -----------------------------------------

    #[test]
    fn standalone_transaction_control_is_benign() {
        for sql in [
            "BEGIN TRANSACTION",
            "BEGIN TRAN",
            "begin tran",
            "COMMIT",
            "COMMIT TRANSACTION",
            "ROLLBACK",
            "ROLLBACK TRANSACTION",
            "SAVE TRANSACTION sp1",
            "START TRANSACTION",
        ] {
            let v = classify_rw(sql);
            assert_eq!(v.level, GuardLevel::Info, "{sql:?} should be benign");
            assert!(v.reasons.is_empty(), "{sql:?} should have no reasons");
        }
    }

    #[test]
    fn rolled_back_transaction_around_select_is_info() {
        // The reported bug: BEGIN TRAN … ROLLBACK around a read no longer yields
        // a spurious "unrecognised statement" confirm.
        let v = classify_rw("BEGIN TRANSACTION;\nSELECT * FROM t;\nROLLBACK;");
        assert_eq!(v.level, GuardLevel::Info);
        assert!(v.reasons.is_empty());
    }

    #[test]
    fn rolled_back_transaction_around_update_is_info() {
        // Dry-run DML wrapped in an explicit rollback should not show the
        // destructive-SQL modal. The same UPDATE with COMMIT is tested below.
        let v = classify_rw("BEGIN TRAN;\nUPDATE t SET x = 1 WHERE id = 1;\nROLLBACK;");
        assert_eq!(v.level, GuardLevel::Info);
        assert!(v.reasons.is_empty());
    }

    #[test]
    fn no_semicolon_rollback_wrap_suppresses_guard() {
        // Users often write T-SQL without semicolons. The rollback-wrap exemption
        // must also fire for these batches.
        let v = classify_rw("BEGIN TRANSACTION\nUPDATE t SET x = 1 WHERE id = 1\nROLLBACK");
        assert_eq!(v.level, GuardLevel::Info);
        assert!(v.reasons.is_empty());

        // ROLLBACK TRANSACTION variant.
        let v = classify_rw("BEGIN TRAN\nDELETE FROM t WHERE id = 1\nROLLBACK TRANSACTION");
        assert_eq!(v.level, GuardLevel::Info);
        assert!(v.reasons.is_empty());

        // Multiple DML statements inside the wrapper.
        let v = classify_rw(
            "BEGIN TRAN\nUPDATE a SET x = 1 WHERE id = 1\nDELETE FROM b WHERE id = 2\nROLLBACK",
        );
        assert_eq!(v.level, GuardLevel::Info);
        assert!(v.reasons.is_empty());
    }

    #[test]
    fn no_semicolon_rollback_wrap_still_warns_for_ddl_and_unknown() {
        // DDL inside a no-semicolon rollback wrapper must still warn.
        let ddl = classify_rw("BEGIN TRAN\nDROP TABLE t\nROLLBACK");
        assert_eq!(ddl.level, GuardLevel::Confirm);
        assert!(ddl.reasons.iter().any(|r| r.contains("DROP")));

        // EXEC inside a no-semicolon rollback wrapper must still warn.
        let exec = classify_rw("BEGIN TRAN\nEXEC sp_something\nROLLBACK");
        assert_eq!(exec.level, GuardLevel::Confirm);

        // Savepoint ROLLBACK (ROLLBACK TRAN sp1) is not a full rollback — warn.
        let savepoint =
            classify_rw("BEGIN TRAN\nUPDATE t SET x = 1 WHERE id = 1\nROLLBACK TRAN sp1");
        assert_eq!(savepoint.level, GuardLevel::Confirm);

        // No trailing ROLLBACK: normal classification applies.
        let no_rb = classify_rw("BEGIN TRAN UPDATE t SET x = 1");
        assert_eq!(no_rb.level, GuardLevel::Confirm);
    }

    #[test]
    fn backup_and_restore_are_recognised_ddl_not_unrecognised() {
        // Hand-typed BACKUP/RESTORE in the editor confirm with a specific reason
        // (not the generic "unrecognised statement"), and are blocked read-only.
        let backup = classify_rw("BACKUP DATABASE [Sales] TO DISK = N'/tmp/s.bak'");
        assert_eq!(backup.level, GuardLevel::Confirm);
        assert!(backup.reasons.iter().any(|r| r.contains("BACKUP")));
        assert!(!backup.reasons.iter().any(|r| r.contains("unrecognised")));

        let restore = classify_rw("RESTORE DATABASE [Sales] FROM DISK = N'/tmp/s.bak'");
        assert_eq!(restore.level, GuardLevel::Confirm);
        assert!(restore.reasons.iter().any(|r| r.contains("RESTORE")));

        // Read-only mode blocks both (they are not reads).
        let ro = classify("RESTORE DATABASE [Sales] FROM DISK = N'/tmp/s.bak'", true);
        assert_eq!(ro.level, GuardLevel::Block);
        assert!(ro.reasons.iter().any(|r| r.contains("read-only mode")));
    }

    #[test]
    fn transaction_wrapped_update_confirms_via_the_update_not_unrecognised() {
        // The committed case still confirms — but because of the UPDATE, with an
        // accurate reason, never the old "unrecognised statement".
        let v = classify_rw("BEGIN TRAN;\nUPDATE t SET x = 1 WHERE id = 1;\nCOMMIT;");
        assert_eq!(v.level, GuardLevel::Confirm);
        assert!(v.reasons.iter().any(|r| r.contains("modifies data")));
        assert!(!v.reasons.iter().any(|r| r.contains("unrecognised")));
    }

    #[test]
    fn no_semicolon_begin_tran_still_sees_inner_update() {
        // A single fragment (no `;`): the leading BEGIN TRAN is peeled and the
        // inner UPDATE is still classified — no false benign downgrade.
        let v = classify_rw("BEGIN TRAN UPDATE t SET x = 1");
        assert_eq!(v.level, GuardLevel::Confirm);
        assert!(v
            .reasons
            .iter()
            .any(|r| r.contains("without WHERE affects all rows")));
    }

    #[test]
    fn bare_begin_block_stays_cautious() {
        // A bare BEGIN (block start) is NOT transaction control; keep it
        // cautious so a DELETE hidden in a BEGIN…END block isn't downgraded.
        let v = classify_rw("BEGIN DELETE FROM t END");
        assert_eq!(v.level, GuardLevel::Confirm);
    }

    #[test]
    fn rollback_wrapper_does_not_hide_ddl_or_unknown_statements() {
        let ddl = classify_rw("BEGIN TRAN;\nDROP TABLE t;\nROLLBACK;");
        assert_eq!(ddl.level, GuardLevel::Confirm);
        assert!(ddl.reasons.iter().any(|r| r.contains("DROP")));

        let unknown = classify_rw("BEGIN TRAN;\nVACUUM;\nROLLBACK;");
        assert_eq!(unknown.level, GuardLevel::Confirm);
        assert!(unknown.reasons.iter().any(|r| r.contains("unrecognised")));

        let savepoint = classify_rw("BEGIN TRAN;\nUPDATE t SET x = 1;\nROLLBACK TRAN sp1;");
        assert_eq!(savepoint.level, GuardLevel::Confirm);
        assert!(savepoint.reasons.iter().any(|r| r.contains("UPDATE")));
    }

    #[test]
    fn read_only_allows_transaction_control_but_blocks_inner_write() {
        assert_eq!(classify("BEGIN TRAN", true).level, GuardLevel::Info);
        assert_eq!(classify("ROLLBACK", true).level, GuardLevel::Info);
        let v = classify(
            "BEGIN TRAN;\nUPDATE t SET x = 1 WHERE id = 1;\nCOMMIT;",
            true,
        );
        assert_eq!(v.level, GuardLevel::Block);
        assert!(v.reasons.iter().any(|r| r.contains("read-only mode")));

        let rolled_back = classify(
            "BEGIN TRAN;\nUPDATE t SET x = 1 WHERE id = 1;\nROLLBACK;",
            true,
        );
        assert_eq!(rolled_back.level, GuardLevel::Block);
        assert!(rolled_back
            .reasons
            .iter()
            .any(|r| r.contains("read-only mode")));
    }

    // --- countable-DML routing (affected-row counts) -----------------------

    #[test]
    fn countable_dml_batches_are_detected() {
        assert!(is_countable_dml_batch("UPDATE t SET x = 1 WHERE id = 1"));
        assert!(is_countable_dml_batch("DELETE FROM t"));
        assert!(is_countable_dml_batch("INSERT INTO t (a) VALUES (1)"));
        // INSERT…SELECT returns no result set — still countable.
        assert!(is_countable_dml_batch("INSERT INTO t (a) SELECT a FROM s"));
        // Several DML statements in one batch (per-statement counts).
        assert!(is_countable_dml_batch("UPDATE a SET x = 1; DELETE FROM b"));
        assert!(is_countable_dml_batch("delete from t;")); // trailing ; + lower-case
    }

    #[test]
    fn rollback_wrapped_dml_is_countable() {
        // Semicolon-separated form.
        assert!(is_countable_dml_batch(
            "BEGIN TRAN;\nUPDATE t SET x = 1 WHERE id = 1;\nROLLBACK;"
        ));
        assert!(is_countable_dml_batch(
            "BEGIN TRANSACTION;\nDELETE FROM t WHERE id = 1;\nROLLBACK TRANSACTION;"
        ));
        // Multiple inner DML statements — one count per statement.
        assert!(is_countable_dml_batch(
            "BEGIN TRAN;\nUPDATE a SET x = 1;\nDELETE FROM b;\nROLLBACK;"
        ));

        // No-semicolon form (common T-SQL style).
        assert!(is_countable_dml_batch(
            "BEGIN TRANSACTION\nUPDATE t SET x = 1 WHERE id = 1\nROLLBACK"
        ));
        assert!(is_countable_dml_batch(
            "BEGIN TRAN\nDELETE FROM t WHERE id = 1\nROLLBACK TRAN"
        ));

        // Committed wrappers and non-DML inner content must remain non-countable.
        assert!(!is_countable_dml_batch(
            "BEGIN TRAN;\nUPDATE t SET x = 1;\nCOMMIT;"
        ));
        assert!(!is_countable_dml_batch(
            "BEGIN TRAN;\nSELECT * FROM t;\nROLLBACK;"
        ));
        assert!(!is_countable_dml_batch(
            "BEGIN TRAN\nSELECT * FROM t\nROLLBACK"
        ));
        // DML with OUTPUT inside rollback wrapper — rows would be discarded.
        assert!(!is_countable_dml_batch(
            "BEGIN TRAN;\nUPDATE t SET x = 1 OUTPUT inserted.id WHERE id = 1;\nROLLBACK;"
        ));
        // Bare wrapper with no inner DML is not countable.
        assert!(!is_countable_dml_batch("BEGIN TRAN;\nROLLBACK;"));
        assert!(!is_countable_dml_batch("BEGIN TRAN\nROLLBACK"));
    }

    #[test]
    fn non_countable_batches_stay_on_the_streaming_path() {
        // Returns rows / not pure DML / mixed / transactional / empty.
        assert!(!is_countable_dml_batch("SELECT * FROM t"));
        assert!(!is_countable_dml_batch(
            "UPDATE t SET x = 1 OUTPUT inserted.id WHERE id = 1"
        ));
        assert!(!is_countable_dml_batch(
            "WITH c AS (SELECT 1) DELETE FROM c"
        ));
        assert!(!is_countable_dml_batch("EXEC some_proc"));
        assert!(!is_countable_dml_batch("CREATE TABLE t (id INT)"));
        assert!(!is_countable_dml_batch("SELECT 1; UPDATE t SET x = 1"));
        assert!(!is_countable_dml_batch(
            "BEGIN TRAN; UPDATE t SET x = 1; COMMIT"
        ));
        assert!(!is_countable_dml_batch(""));
        assert!(!is_countable_dml_batch("-- just a comment"));
        // A column literally named with an "output" substring must not trip the
        // word-boundary OUTPUT check.
        assert!(is_countable_dml_batch(
            "UPDATE t SET output_flag = 1 WHERE id = 1"
        ));
    }

    #[test]
    fn is_rollback_wrapped_dml_batch_recognises_both_forms() {
        // Semicolon-separated.
        assert!(is_rollback_wrapped_dml_batch(
            "BEGIN TRAN;\nUPDATE t SET x = 1 WHERE id = 1;\nROLLBACK;"
        ));
        assert!(is_rollback_wrapped_dml_batch(
            "BEGIN TRANSACTION;\nDELETE FROM t;\nROLLBACK TRANSACTION;"
        ));
        // No-semicolon form.
        assert!(is_rollback_wrapped_dml_batch(
            "BEGIN TRAN\nUPDATE t SET x = 1 WHERE id = 1\nROLLBACK"
        ));
        // Plain DML — NOT rollback-wrapped.
        assert!(!is_rollback_wrapped_dml_batch(
            "UPDATE t SET x = 1 WHERE id = 1"
        ));
        assert!(!is_rollback_wrapped_dml_batch(
            "DELETE FROM t; INSERT INTO t VALUES (1)"
        ));
        // Committed wrapper — NOT a dry-run.
        assert!(!is_rollback_wrapped_dml_batch(
            "BEGIN TRAN; UPDATE t SET x = 1; COMMIT"
        ));
        // SELECT inside wrapper — discarded by execute(), so not eligible.
        assert!(!is_rollback_wrapped_dml_batch(
            "BEGIN TRAN; SELECT * FROM t; ROLLBACK"
        ));
        // Empty / comment-only.
        assert!(!is_rollback_wrapped_dml_batch(""));
        assert!(!is_rollback_wrapped_dml_batch("-- nothing here"));
    }

    #[test]
    fn dml_with_variable_ops_is_countable() {
        // DECLARE + DML is countable.
        assert!(is_countable_dml_batch(
            "DECLARE @id INT;\nINSERT INTO t (a) VALUES (1)"
        ));
        // DML + SET @var = scalar.
        assert!(is_countable_dml_batch(
            "INSERT INTO t (a) VALUES (1);\nSET @id = SCOPE_IDENTITY()"
        ));
        // DML + SET @var = scalar subquery — the pattern the user reported.
        assert!(is_countable_dml_batch(
            "INSERT INTO t (a) VALUES (1);\nSET @id = (SELECT SCOPE_IDENTITY())"
        ));
        // Full pattern: DECLARE + INSERT + SET + INSERT.
        assert!(is_countable_dml_batch(
            "DECLARE @id INT;\nINSERT INTO a (x) VALUES (1);\nSET @id = (SELECT SCOPE_IDENTITY());\nINSERT INTO b (a_id) VALUES (@id)"
        ));
        // Variable-ops only (no DML) — nothing to count.
        assert!(!is_countable_dml_batch("DECLARE @x INT"));
        assert!(!is_countable_dml_batch("SET @x = 1"));
        assert!(!is_countable_dml_batch("DECLARE @x INT;\nSET @x = 1"));
        // SELECT mixed in — stays on the streaming path (would return rows).
        assert!(!is_countable_dml_batch(
            "INSERT INTO t VALUES (1);\nSELECT * FROM t"
        ));
        // USE is Kind::Read but not DECLARE/SET — stays on streaming path.
        assert!(!is_countable_dml_batch(
            "USE mydb;\nINSERT INTO t VALUES (1)"
        ));
    }

    #[test]
    fn rollback_wrapped_dml_with_variable_ops_is_countable() {
        // DECLARE + INSERT + SET @var inside a rollback wrapper.
        assert!(is_countable_dml_batch(
            "BEGIN TRAN;\nDECLARE @id INT;\nINSERT INTO t (a) VALUES (1);\nSET @id = (SELECT SCOPE_IDENTITY());\nROLLBACK;"
        ));
        // Only DECLARE/SET inside wrapper (no DML) — not countable.
        assert!(!is_countable_dml_batch(
            "BEGIN TRAN;\nDECLARE @x INT;\nSET @x = 1;\nROLLBACK;"
        ));
        // SELECT inside wrapper stays non-countable.
        assert!(!is_countable_dml_batch(
            "BEGIN TRAN;\nDECLARE @x INT;\nSELECT @x = 1;\nROLLBACK;"
        ));
    }

    #[test]
    fn no_semicolon_begin_tran_glued_to_first_inner_is_countable() {
        // The reported pattern: `BEGIN TRANSACTION` on its own line (no `;`), so
        // the split glues it to the first inner statement. The opener must be
        // peeled off the first statement, not required to stand alone.
        assert!(is_countable_dml_batch(
            "BEGIN TRANSACTION\nDECLARE @id INT;\nINSERT INTO t (a) VALUES (1);\nROLLBACK"
        ));
        // Glued opener + inline INSERT as the first inner statement.
        assert!(is_countable_dml_batch(
            "BEGIN TRAN INSERT INTO t (a) VALUES (1);\nINSERT INTO u (a) VALUES (2);\nROLLBACK"
        ));
        // And the matching dry-run trimmer recognises it too.
        assert!(is_rollback_wrapped_dml_batch(
            "BEGIN TRANSACTION\nDECLARE @id INT;\nINSERT INTO t (a) VALUES (1);\nROLLBACK"
        ));
    }

    #[test]
    fn glued_opener_rollback_wrap_is_info_in_classify() {
        // Same glued-opener shape on the classify side: a rolled-back dry-run
        // must not surface the destructive-SQL confirm.
        let v = classify_rw(
            "BEGIN TRANSACTION\nDECLARE @id INT;\nINSERT INTO t (a) VALUES (1);\nROLLBACK",
        );
        assert_eq!(v.level, GuardLevel::Info);
        assert!(v.reasons.is_empty());
    }

    // --- peel_leading_use ---------------------------------------------------

    #[test]
    fn peel_leading_use_splits_the_users_batch() {
        // The exact shape the user reported: a leading `USE`, then a
        // rollback-wrapped INSERT batch. The USE is peeled; the remainder is
        // countable DML so it routes to the affected-count path.
        let sql = "-- localhost\n\nUSE web02;\n\nBEGIN TRANSACTION\nDECLARE @id INT;\nINSERT INTO dbo.t (a) VALUES (1);\nSET @id = (SELECT SCOPE_IDENTITY());\nINSERT INTO dbo.u (b) SELECT @id;\nROLLBACK";
        let (uses, remainder) = peel_leading_use(sql).expect("leading USE peeled");
        assert_eq!(uses.len(), 1);
        assert!(uses[0].contains("USE web02"));
        assert!(remainder.trim_start().starts_with("BEGIN TRANSACTION"));
        assert!(is_countable_dml_batch(remainder));
    }

    #[test]
    fn peel_leading_use_handles_multiple_and_comments() {
        let (uses, remainder) =
            peel_leading_use("USE a;\n/* c */ USE b;\nINSERT INTO t (x) VALUES (1)")
                .expect("two leading USEs peeled");
        assert_eq!(uses.len(), 2);
        assert!(remainder.trim_start().starts_with("INSERT"));
    }

    #[test]
    fn peel_leading_use_none_when_no_leading_use() {
        // No leading USE → None (batch handled by the normal dispatch).
        assert!(peel_leading_use("INSERT INTO t (a) VALUES (1)").is_none());
        // A USE inside a string literal must not count as a leading USE.
        assert!(peel_leading_use("INSERT INTO t (a) VALUES ('USE x'); ROLLBACK").is_none());
        // A bare `USE db` with no trailing `;` is left in place (None) so the
        // normal path runs it (where USE persists context anyway).
        assert!(peel_leading_use("USE web02").is_none());
    }

    #[test]
    fn peel_leading_use_only_use_leaves_blank_remainder() {
        let (uses, remainder) = peel_leading_use("USE web02;").expect("USE peeled");
        assert_eq!(uses.len(), 1);
        assert!(remainder.trim().is_empty());
    }
}
