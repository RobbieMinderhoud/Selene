//! A heuristic MongoDB safety classifier — the document-store sibling of
//! [`sql_guard`](super::sql_guard).
//!
//! Selene accepts MongoDB "queries" in the mongosh-shell shape
//! `db.<collection>.<method>(<args>)[.<chain>(<args>)]*` (see the driver's
//! `query` parser). Unlike SQL there is no leading keyword to inspect — the risk
//! is carried by the **method name** (`find` reads, `deleteMany` writes) and, for
//! `aggregate`, by whether the pipeline ends in a persisting `$out`/`$merge`
//! stage. This classifier extracts that method name with a small string-aware
//! tokenizer and maps it to a [`GuardVerdict`], honouring the connection's
//! `read_only` flag exactly as the SQL guard does.
//!
//! **Dependency-free by design.** This module deliberately does *not* depend on
//! the `mongodb`/`bson` crates (nor on the feature-gated driver's `query`
//! module): it parses the method name straight from the string. That keeps the
//! guard — and the [`classify_for`](super::classify_for) dispatcher that calls it
//! — compilable in the default (mongodb-off) build, so `src-tauri` can enforce it
//! server-side without pulling in the driver.
//!
//! Like the SQL guard this is a heuristic, not a parser: it is intentionally
//! conservative (an unrecognised or unparseable method leans toward
//! confirm/block, never toward "benign").

use super::sql_guard::{GuardLevel, GuardVerdict};

/// Classify a mongosh-subset query for safety, honouring the connection's
/// `read_only` flag.
///
/// - **Read** methods (`find`, `aggregate` without a `$out`/`$merge`, …) are
///   [`GuardLevel::Info`].
/// - **Write** methods (`insertOne`, `deleteMany`, `drop`, an `aggregate` that
///   persists via `$out`/`$merge`, …) are [`GuardLevel::Confirm`] normally and
///   [`GuardLevel::Block`] on a read-only connection.
/// - An **unknown / unparseable** method is [`GuardLevel::Confirm`] normally and
///   [`GuardLevel::Block`] read-only — the same caution the SQL guard applies to
///   an unrecognised statement.
/// - Empty / whitespace-only input is a benign [`GuardLevel::Info`].
pub fn classify_mongo(query: &str, read_only: bool) -> GuardVerdict {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return GuardVerdict::benign();
    }

    match method_name(trimmed) {
        Some(method) => classify_method(trimmed, method, read_only),
        // Not the `db.<coll>.<method>(…)` shape we recognise. Be conservative:
        // confirm when writable, block when read-only (same as an unknown method).
        None => unknown_verdict(read_only),
    }
}

/// Classify a recognised (or at least parseable) method name.
fn classify_method(query: &str, method: &str, read_only: bool) -> GuardVerdict {
    // A plain read: allowed unconditionally (read-only never blocks a read).
    if is_read_method(method) {
        // `aggregate` is the one read method that can *write*: a terminal
        // `$out`/`$merge` stage persists its output to a collection.
        if method == "aggregate" && aggregate_persists(query) {
            return write_verdict("aggregate $out/$merge writes to a collection", read_only);
        }
        return GuardVerdict::benign();
    }

    // A recognised write / DDL method.
    if let Some(reason) = write_reason(method) {
        return write_verdict(reason, read_only);
    }

    // Parseable shape but an unrecognised method name: confirm / block.
    unknown_verdict(read_only)
}

/// The [`GuardVerdict`] for a write: [`GuardLevel::Confirm`] normally, escalated
/// to [`GuardLevel::Block`] with the read-only reason attached when the
/// connection is read-only. The write's own `reason` is always recorded so the
/// UI can explain *what* it refused.
fn write_verdict(reason: &str, read_only: bool) -> GuardVerdict {
    let mut reasons = vec![reason.to_string()];
    let level = if read_only {
        // Mirror the SQL guard's phrasing so the two backends read alike.
        reasons.push("connection is in read-only mode".to_string());
        GuardLevel::Block
    } else {
        GuardLevel::Confirm
    };
    GuardVerdict { level, reasons }
}

/// The verdict for an unknown / unparseable method: confirm when writable, block
/// when read-only — the same caution the SQL guard applies to an unrecognised
/// statement (never downgraded to benign).
fn unknown_verdict(read_only: bool) -> GuardVerdict {
    if read_only {
        GuardVerdict {
            level: GuardLevel::Block,
            reasons: vec![
                "unrecognised query; review before running".to_string(),
                "connection is in read-only mode".to_string(),
            ],
        }
    } else {
        GuardVerdict {
            level: GuardLevel::Confirm,
            reasons: vec!["unrecognised query; review before running".to_string()],
        }
    }
}

/// Whether `method` is a read-only helper. `aggregate` is included here but is
/// re-checked for a persisting `$out`/`$merge` stage by the caller.
fn is_read_method(method: &str) -> bool {
    matches!(
        method,
        "find"
            | "findOne"
            | "countDocuments"
            | "count"
            | "estimatedDocumentCount"
            | "distinct"
            | "aggregate"
            | "listCollections"
            | "listIndexes"
            | "getIndexes"
            | "getIndexKeys"
    )
}

/// If `method` is a recognised write / DDL operation, the human-readable reason
/// (mirroring the SQL guard's voice, e.g. `"deleteMany writes to the
/// collection"`). `None` for a method that is not a recognised write.
fn write_reason(method: &str) -> Option<&'static str> {
    let reason = match method {
        "insertOne" | "insertMany" => "insert writes to the collection",
        "updateOne" | "updateMany" => "update writes to the collection",
        "deleteOne" | "deleteMany" => "delete writes to the collection",
        "replaceOne" => "replaceOne writes to the collection",
        "findOneAndUpdate" | "findOneAndReplace" | "findOneAndDelete" => {
            "findOneAnd… writes to the collection"
        }
        "bulkWrite" => "bulkWrite writes to the collection",
        "drop" => "drop permanently removes the collection",
        "dropDatabase" => "dropDatabase permanently removes the database",
        "createIndex" | "createIndexes" => "createIndex modifies the collection's indexes",
        "renameCollection" => "renameCollection renames the collection",
        _ => return None,
    };
    Some(reason)
}

/// Whether an `aggregate` pipeline contains a `$out` or `$merge` stage, which
/// persists the pipeline's output to a collection (making it a write).
///
/// A `$out`/`$merge` may legally appear only as the *last* pipeline stage, so a
/// plain search for those stage keys anywhere in the query is a sound signal that
/// the pipeline persists. This is deliberately *not* a JSON parse: a false
/// positive (e.g. the literal text `$out` inside a string value) only forces a
/// confirm — the conservative direction — never a wrongful allow.
fn aggregate_persists(query: &str) -> bool {
    contains_stage_key(query, "$out") || contains_stage_key(query, "$merge")
}

/// Whether the stage key `needle` (`$out`/`$merge`) appears in `haystack` as a
/// complete identifier.
///
/// Because the pipeline is strict JSON, the stage key is itself quoted
/// (`"$out"`), so we match the raw bytes case-insensitively rather than trying to
/// skip string literals — matching inside the quotes is exactly what we want. The
/// only refinement is an identifier boundary *after* the needle so a longer
/// operator (`$outer`, `$mergeObjects`) does not match the shorter key.
fn contains_stage_key(haystack: &str, needle: &str) -> bool {
    let hay = haystack.as_bytes();
    let need = needle.as_bytes();
    if need.is_empty() || hay.len() < need.len() {
        return false;
    }
    // A byte that can continue a Mongo field identifier after the stage key.
    let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';

    let mut i = 0;
    while i + need.len() <= hay.len() {
        if hay[i..i + need.len()].eq_ignore_ascii_case(need) {
            let after = i + need.len();
            // `$out`/`$merge` are complete keys: the following byte must not be a
            // continuation identifier byte (rules out `$outer`, `$mergeObjects`).
            let after_ok = after >= hay.len() || !is_ident(hay[after]);
            if after_ok {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// Extract the **first method name** from a mongosh-subset query
/// (`db.<collection>.<method>(…)`), or `None` when the string does not have that
/// shape.
///
/// This is a small, string-aware scan — deliberately independent of the driver's
/// (feature-gated, bson-backed) `query` parser so the guard stays dependency-free
/// and compiles in the mongodb-off build. It only needs the method *name*, so it
/// does not parse arguments; it walks the leading `db` handle, skips the bare
/// collection segment, and returns the identifier of the first `.<name>(` call.
fn method_name(query: &str) -> Option<&str> {
    let bytes = query.as_bytes();

    // Leading handle must be exactly `db`.
    let handle_end = ident_end(bytes, 0);
    if &query[..handle_end] != "db" {
        return None;
    }
    let mut i = handle_end;

    // Walk `.segment` runs. The first *bare* segment (no `(`) is the collection;
    // the first segment *followed by* `(` is the method we want.
    while i < bytes.len() {
        // Tolerate stray whitespace / a trailing `;`.
        if bytes[i].is_ascii_whitespace() || bytes[i] == b';' {
            i += 1;
            continue;
        }
        if bytes[i] != b'.' {
            return None;
        }
        i += 1; // consume '.'

        let name_start = i;
        let name_end = ident_end(bytes, i);
        if name_end == name_start {
            return None; // `.` with no identifier
        }
        let name = &query[name_start..name_end];
        i = name_end;

        // Is this segment a call (`.name(`)? Skip whitespace, then look for `(`.
        let mut j = i;
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        if j < bytes.len() && bytes[j] == b'(' {
            // First call in the chain → its name is the method.
            return Some(name);
        }
        // A bare segment (the collection); keep scanning for the method call.
    }
    None
}

/// The end index of an identifier run starting at `start` (letters, digits, `_`,
/// `$`), matching the driver tokenizer's identifier rule. A `.` ends the run (it
/// is the chain separator).
fn ident_end(bytes: &[u8], start: usize) -> usize {
    let mut i = start;
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_alphanumeric() || b == b'_' || b == b'$' {
            i += 1;
        } else {
            break;
        }
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- read methods → Info (writable and read-only) ----------------------

    #[test]
    fn read_methods_are_info_writable_and_read_only() {
        for q in [
            "db.orders.find({})",
            r#"db.orders.find({ "status": "open" }).sort({ "n": -1 }).limit(20)"#,
            "db.orders.findOne({})",
            "db.orders.countDocuments({})",
            r#"db.orders.count({ "a": 1 })"#,
            r#"db.orders.distinct("status")"#,
            "db.orders.estimatedDocumentCount()",
            "db.orders.listIndexes()",
            "db.orders.getIndexes()",
            "db.system.listCollections()",
        ] {
            assert_eq!(
                classify_mongo(q, false).level,
                GuardLevel::Info,
                "{q:?} should be Info when writable"
            );
            // A read is allowed even on a read-only connection.
            assert_eq!(
                classify_mongo(q, true).level,
                GuardLevel::Info,
                "{q:?} should be Info even read-only"
            );
        }
    }

    #[test]
    fn plain_aggregate_is_info() {
        let q = r#"db.orders.aggregate([{ "$match": { "active": true } }, { "$group": { "_id": "$c" } }])"#;
        assert_eq!(classify_mongo(q, false).level, GuardLevel::Info);
        assert_eq!(classify_mongo(q, true).level, GuardLevel::Info);
    }

    // --- write methods → Confirm (writable), Block (read-only) --------------

    #[test]
    fn write_methods_confirm_when_writable_and_block_read_only() {
        for (q, needle) in [
            (r#"db.c.insertOne({ "a": 1 })"#, "insert"),
            (r#"db.c.insertMany([{ "a": 1 }])"#, "insert"),
            (r#"db.c.updateOne({}, { "$set": { "a": 1 } })"#, "update"),
            (r#"db.c.updateMany({}, { "$set": { "a": 1 } })"#, "update"),
            (r#"db.c.deleteOne({ "a": 1 })"#, "delete"),
            ("db.c.deleteMany({})", "delete"),
            (r#"db.c.replaceOne({}, {})"#, "replaceOne"),
            (r#"db.c.findOneAndUpdate({}, {})"#, "findOneAnd"),
            (r#"db.c.findOneAndReplace({}, {})"#, "findOneAnd"),
            (r#"db.c.findOneAndDelete({})"#, "findOneAnd"),
            (r#"db.c.bulkWrite([])"#, "bulkWrite"),
            ("db.c.drop()", "drop"),
            ("db.c.dropDatabase()", "dropDatabase"),
            (r#"db.c.createIndex({ "a": 1 })"#, "createIndex"),
            (r#"db.c.createIndexes([{ "a": 1 }])"#, "createIndex"),
            (r#"db.c.renameCollection("d")"#, "renameCollection"),
        ] {
            let w = classify_mongo(q, false);
            assert_eq!(
                w.level,
                GuardLevel::Confirm,
                "{q:?} should Confirm writable"
            );
            assert!(
                w.reasons.iter().any(|r| r.contains(needle)),
                "{q:?} reasons {:?} should mention {needle}",
                w.reasons
            );
            assert!(
                !w.reasons.iter().any(|r| r.contains("read-only")),
                "{q:?} must not claim read-only when writable"
            );

            let ro = classify_mongo(q, true);
            assert_eq!(ro.level, GuardLevel::Block, "{q:?} should Block read-only");
            assert!(
                ro.reasons.iter().any(|r| r.contains("read-only mode")),
                "{q:?} read-only reasons {:?} should mention read-only",
                ro.reasons
            );
            // The specific write reason is still recorded alongside the block.
            assert!(ro.reasons.iter().any(|r| r.contains(needle)));
        }
    }

    // --- aggregate $out / $merge → write ------------------------------------

    #[test]
    fn aggregate_with_out_or_merge_is_a_write() {
        for q in [
            r#"db.c.aggregate([{ "$match": {} }, { "$out": "archive" }])"#,
            r#"db.c.aggregate([{ "$merge": { "into": "archive" } }])"#,
        ] {
            let w = classify_mongo(q, false);
            assert_eq!(
                w.level,
                GuardLevel::Confirm,
                "{q:?} should Confirm writable"
            );
            assert!(
                w.reasons.iter().any(|r| r.contains("$out/$merge")),
                "{q:?} reasons {:?} should mention $out/$merge",
                w.reasons
            );

            let ro = classify_mongo(q, true);
            assert_eq!(ro.level, GuardLevel::Block, "{q:?} should Block read-only");
            assert!(ro.reasons.iter().any(|r| r.contains("read-only mode")));
        }
    }

    #[test]
    fn aggregate_out_substring_in_other_stage_does_not_false_match() {
        // `$outfielded` / `$mergeObjects` are not the persisting stages; the
        // identifier-boundary check keeps a plain aggregate Info.
        let q = r#"db.c.aggregate([{ "$project": { "x": { "$mergeObjects": ["$a", "$b"] } } }])"#;
        assert_eq!(classify_mongo(q, false).level, GuardLevel::Info);
    }

    // --- unknown / unparseable ----------------------------------------------

    #[test]
    fn unknown_method_confirms_writable_blocks_read_only() {
        let q = "db.c.frobnicate({})";
        assert_eq!(classify_mongo(q, false).level, GuardLevel::Confirm);
        assert_eq!(classify_mongo(q, true).level, GuardLevel::Block);
    }

    #[test]
    fn non_db_string_confirms_writable_blocks_read_only() {
        for q in ["not a query", "foo.bar.find({})", "{ \"just\": \"json\" }"] {
            assert_eq!(
                classify_mongo(q, false).level,
                GuardLevel::Confirm,
                "{q:?} should Confirm writable"
            );
            assert_eq!(
                classify_mongo(q, true).level,
                GuardLevel::Block,
                "{q:?} should Block read-only"
            );
        }
    }

    // --- empty ---------------------------------------------------------------

    #[test]
    fn empty_and_whitespace_are_info() {
        for q in ["", "   ", "\n\t "] {
            let v = classify_mongo(q, false);
            assert_eq!(v.level, GuardLevel::Info, "{q:?} should be benign");
            assert!(v.reasons.is_empty());
            // Even read-only: an empty query writes nothing.
            assert_eq!(classify_mongo(q, true).level, GuardLevel::Info);
        }
    }

    // --- method_name tokenizer ----------------------------------------------

    #[test]
    fn method_name_extracts_the_first_call() {
        assert_eq!(method_name("db.orders.find({})"), Some("find"));
        assert_eq!(
            method_name("db.orders.find({}).sort({}).limit(5)"),
            Some("find")
        );
        assert_eq!(method_name("db.a.deleteMany({}) ;"), Some("deleteMany"));
        // Collection names with `$`/`_`/digits are fine.
        assert_eq!(
            method_name("db.a_b$1.countDocuments()"),
            Some("countDocuments")
        );
    }

    #[test]
    fn method_name_rejects_non_db_shapes() {
        assert_eq!(method_name("foo.c.find({})"), None);
        assert_eq!(method_name("db"), None);
        assert_eq!(method_name("db.c"), None); // no call
        assert_eq!(method_name(""), None);
    }
}
