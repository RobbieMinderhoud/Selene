//! A parser for the **mongosh shell subset** Selene accepts as a MongoDB
//! "query": `db.<collection>.<method>(<args>)[.<chain>(<args>)]*`.
//!
//! This is deliberately *not* a full JavaScript parser. mongosh queries in the
//! editor take the familiar shape
//!
//! ```text
//! db.orders.find({ status: "open" }).sort({ createdAt: -1 }).limit(20)
//! db.orders.aggregate([{ $match: { status: "open" } }, { $group: { _id: "$c" } }])
//! db.orders.countDocuments({ status: "open" })
//! db.orders.distinct("status", { region: "eu" })
//! ```
//!
//! Parsing is two-staged:
//!
//! 1. A **hand tokenizer** ([`split_calls`]) walks the byte string, finding the
//!    `db.<coll>.<method>` prefix and every chained `.<name>(...)` call. Matching
//!    parens/brackets/braces are located by depth-counting that *respects string
//!    literals* (single- and double-quoted, with `\` escapes) — reusing the
//!    discipline from `guard::sql_guard` so a `)`/`}` inside a string never
//!    prematurely closes a call.
//! 2. **Argument parsing** ([`parse_json_arg`]) runs each argument through
//!    `serde_json` → [`bson::Bson`] (via [`bson::to_bson`]).
//!
//! ## v1 argument grammar: **strict JSON only**
//!
//! Arguments must be valid *strict* JSON — double-quoted keys and strings, no
//! trailing commas, no unquoted identifiers, and no shell helpers such as
//! `ObjectId("…")` or `ISODate("…")`. The relaxed-JSON / extended-JSON shim
//! (unquoted keys, `ObjectId(...)`, single-quoted strings inside args) is a
//! deliberately-deferred later change. A malformed argument yields a
//! [`CoreError::Query`] naming the offending call.

use bson::Bson;

use crate::error::CoreError;

/// A parsed mongosh query — the read methods plus the core write methods.
///
/// Writes emit a column-less affected-count result set (see `super::writes`),
/// mirroring how SQL DML is surfaced. Some higher-level writes remain
/// [`CoreError::Unsupported`] (see [`parse`]).
///
/// The `Find` variant is larger than the others because it carries several
/// `Bson` documents, but this value is short-lived — it is produced once per
/// query in [`parse`] and consumed immediately in `execute`, never stored in a
/// collection — so boxing the fields to equalise variant sizes would add
/// allocation and obscure the deliberately-explicit shape for no real benefit.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum MongoQuery {
    Find {
        collection: String,
        filter: Bson,
        projection: Option<Bson>,
        sort: Option<Bson>,
        skip: Option<u64>,
        limit: Option<i64>,
    },
    Aggregate {
        collection: String,
        pipeline: Vec<Bson>,
    },
    CountDocuments {
        collection: String,
        filter: Bson,
    },
    Distinct {
        collection: String,
        field: String,
        filter: Bson,
    },
    InsertOne {
        collection: String,
        document: Bson,
    },
    InsertMany {
        collection: String,
        documents: Vec<Bson>,
    },
    UpdateOne {
        collection: String,
        filter: Bson,
        update: Bson,
        upsert: bool,
    },
    UpdateMany {
        collection: String,
        filter: Bson,
        update: Bson,
        upsert: bool,
    },
    DeleteOne {
        collection: String,
        filter: Bson,
    },
    DeleteMany {
        collection: String,
        filter: Bson,
    },
    ReplaceOne {
        collection: String,
        filter: Bson,
        replacement: Bson,
        upsert: bool,
    },
    DropCollection {
        collection: String,
    },
}

/// One `.<name>(<raw-args>)` call extracted from the source, with the raw
/// (untrimmed-of-quotes) argument text between the parens preserved verbatim.
#[derive(Debug, Clone)]
struct Call<'a> {
    name: &'a str,
    /// The exact source between the call's `(` and its matching `)`.
    args: &'a str,
}

/// Parse a mongosh-shell-subset query string into a [`MongoQuery`].
///
/// Returns [`CoreError::Query`] for anything not matching the
/// `db.<collection>.<method>(...)` shape or with a malformed argument, and
/// [`CoreError::Unsupported`] for a recognised method Selene does not yet run
/// (the higher-level writes: `findOneAnd…`, `bulkWrite`, `createIndex…`,
/// `renameCollection`, `dropDatabase`).
pub(crate) fn parse(input: &str) -> Result<MongoQuery, CoreError> {
    let src = input.trim();
    if src.is_empty() {
        return Err(CoreError::Query("empty query".into()));
    }

    // --- Stage 1: split into the `db` handle + a chain of `.name(args)` calls.
    let (handle, calls) = split_calls(src)?;
    if handle != "db" {
        return Err(CoreError::Query(format!(
            "query must start with the `db` handle, got `{handle}`"
        )));
    }
    // Shape is `db.<collection>.<method>(...)[.<chain>(...)]*`: the collection is
    // the first *bare* segment (no parens) and the method is the first call.
    let (collection, method_calls) = split_collection(&calls)?;

    if method_calls.is_empty() {
        return Err(CoreError::Query(
            "expected a method call after the collection, e.g. `db.coll.find({})`".into(),
        ));
    }
    let method = &method_calls[0];
    let chain = &method_calls[1..];

    // --- Stage 2: dispatch on the method name.
    match method.name {
        "find" => parse_find(collection, method, chain),
        "findOne" => parse_find_one(collection, method, chain),
        "aggregate" => parse_aggregate(collection, method, chain),
        "countDocuments" | "count" => parse_count(collection, method, chain),
        "distinct" => parse_distinct(collection, method, chain),
        // Core writes: parsed here, executed in `super::writes`. The guard
        // (enforced server-side) still Confirms these on a writable connection
        // and Blocks them on a read-only one, so `execute` only runs an approved
        // write.
        "insertOne" => parse_insert_one(collection, method, chain),
        "insertMany" => parse_insert_many(collection, method, chain),
        "updateOne" | "updateMany" => parse_update(collection, method, chain),
        "replaceOne" => parse_replace_one(collection, method, chain),
        "deleteOne" | "deleteMany" => parse_delete(collection, method, chain),
        "drop" => parse_drop(collection, method, chain),
        // Higher-level writes/DDL Selene does not yet run. `findOneAnd…` returns
        // a document (a distinct result shape) and the rest are lower-value;
        // they stay Unsupported for now. The guard still classifies them as
        // writes, so the read-only safety story is unchanged.
        "findOneAndUpdate" | "findOneAndReplace" | "findOneAndDelete" | "bulkWrite"
        | "createIndex" | "createIndexes" | "renameCollection" | "dropDatabase" => Err(
            CoreError::Unsupported(format!("`{}` is not yet supported", method.name)),
        ),
        other => Err(CoreError::Query(format!("unknown method `{other}`"))),
    }
}

// ---------------------------------------------------------------------------
// Method handlers
// ---------------------------------------------------------------------------

/// `find(filter?, projection?)` + optional `.projection`/`.project`/`.sort`/
/// `.skip`/`.limit`/`.toArray`/`.pretty` chain.
fn parse_find(collection: &str, method: &Call, chain: &[Call]) -> Result<MongoQuery, CoreError> {
    let args = split_top_level_args(method.args)?;
    if args.len() > 2 {
        return Err(CoreError::Query(
            "find() accepts at most (filter, projection)".into(),
        ));
    }
    let filter = optional_doc_arg(args.first().copied(), "find")?.unwrap_or_else(empty_doc);
    let mut projection = optional_doc_arg(args.get(1).copied(), "find")?;
    let mut sort = None;
    let mut skip = None;
    let mut limit = None;

    apply_find_chain(chain, &mut projection, &mut sort, &mut skip, &mut limit)?;

    Ok(MongoQuery::Find {
        collection: collection.to_string(),
        filter,
        projection,
        sort,
        skip,
        limit,
    })
}

/// `findOne(filter?, projection?)` is a `find` capped at one document.
fn parse_find_one(
    collection: &str,
    method: &Call,
    chain: &[Call],
) -> Result<MongoQuery, CoreError> {
    let args = split_top_level_args(method.args)?;
    if args.len() > 2 {
        return Err(CoreError::Query(
            "findOne() accepts at most (filter, projection)".into(),
        ));
    }
    let filter = optional_doc_arg(args.first().copied(), "findOne")?.unwrap_or_else(empty_doc);
    let mut projection = optional_doc_arg(args.get(1).copied(), "findOne")?;
    let mut sort = None;
    let mut skip = None;
    // findOne fixes the limit at 1; a chained .limit() would be contradictory,
    // but we still let the chain parse (mongosh ignores extra limits) and force 1.
    let mut limit = Some(1i64);
    apply_find_chain(chain, &mut projection, &mut sort, &mut skip, &mut limit)?;
    limit = Some(1);

    Ok(MongoQuery::Find {
        collection: collection.to_string(),
        filter,
        projection,
        sort,
        skip,
        limit,
    })
}

/// `aggregate([ ...stages... ])` → an `Aggregate` with the pipeline stages.
fn parse_aggregate(
    collection: &str,
    method: &Call,
    chain: &[Call],
) -> Result<MongoQuery, CoreError> {
    reject_terminal_only_chain(chain, "aggregate")?;
    let args = split_top_level_args(method.args)?;
    if args.len() != 1 {
        return Err(CoreError::Query(
            "aggregate() expects a single pipeline array argument".into(),
        ));
    }
    let pipeline_bson = parse_json_arg(args[0], "aggregate")?;
    let Bson::Array(stages) = pipeline_bson else {
        return Err(CoreError::Query(
            "aggregate() argument must be an array of pipeline stages".into(),
        ));
    };
    Ok(MongoQuery::Aggregate {
        collection: collection.to_string(),
        pipeline: stages,
    })
}

/// `countDocuments(filter?)` / `count(filter?)`.
fn parse_count(collection: &str, method: &Call, chain: &[Call]) -> Result<MongoQuery, CoreError> {
    reject_terminal_only_chain(chain, method.name)?;
    let args = split_top_level_args(method.args)?;
    if args.len() > 1 {
        return Err(CoreError::Query(format!(
            "{}() accepts at most a single filter argument",
            method.name
        )));
    }
    let filter = optional_doc_arg(args.first().copied(), method.name)?.unwrap_or_else(empty_doc);
    Ok(MongoQuery::CountDocuments {
        collection: collection.to_string(),
        filter,
    })
}

/// `distinct("field", filter?)` — the first argument is the field name string.
fn parse_distinct(
    collection: &str,
    method: &Call,
    chain: &[Call],
) -> Result<MongoQuery, CoreError> {
    reject_terminal_only_chain(chain, "distinct")?;
    let args = split_top_level_args(method.args)?;
    if args.is_empty() || args.len() > 2 {
        return Err(CoreError::Query(
            "distinct() expects (field, filter?) — a field name is required".into(),
        ));
    }
    let field = match parse_json_arg(args[0], "distinct")? {
        Bson::String(s) => s,
        _ => {
            return Err(CoreError::Query(
                "distinct() first argument must be a field-name string".into(),
            ))
        }
    };
    let filter = optional_doc_arg(args.get(1).copied(), "distinct")?.unwrap_or_else(empty_doc);
    Ok(MongoQuery::Distinct {
        collection: collection.to_string(),
        field,
        filter,
    })
}

// ---------------------------------------------------------------------------
// Write method handlers
// ---------------------------------------------------------------------------

/// `insertOne(document)` — exactly one document argument.
fn parse_insert_one(
    collection: &str,
    method: &Call,
    chain: &[Call],
) -> Result<MongoQuery, CoreError> {
    reject_terminal_only_chain(chain, "insertOne")?;
    let args = split_top_level_args(method.args)?;
    if args.len() != 1 {
        return Err(CoreError::Query(
            "insertOne() expects a single document argument".into(),
        ));
    }
    let document = require_document(args[0], "insertOne")?;
    Ok(MongoQuery::InsertOne {
        collection: collection.to_string(),
        document,
    })
}

/// `insertMany([doc, …])` — a single array argument whose elements are documents.
fn parse_insert_many(
    collection: &str,
    method: &Call,
    chain: &[Call],
) -> Result<MongoQuery, CoreError> {
    reject_terminal_only_chain(chain, "insertMany")?;
    let args = split_top_level_args(method.args)?;
    if args.len() != 1 {
        return Err(CoreError::Query(
            "insertMany() expects a single array-of-documents argument".into(),
        ));
    }
    let Bson::Array(elements) = parse_json_arg(args[0], "insertMany")? else {
        return Err(CoreError::Query(
            "insertMany() argument must be an array of documents `[ { … }, … ]`".into(),
        ));
    };
    // Every element must itself be a document.
    for el in &elements {
        if !matches!(el, Bson::Document(_)) {
            return Err(CoreError::Query(
                "insertMany() array elements must each be a document object `{ … }`".into(),
            ));
        }
    }
    Ok(MongoQuery::InsertMany {
        collection: collection.to_string(),
        documents: elements,
    })
}

/// `updateOne(filter, update, options?)` / `updateMany(...)`. The update doc is
/// passed through verbatim (typically `{ $set: {…} }`); we deliberately do *not*
/// validate that it contains update operators — the server does. The optional
/// third argument is an options doc from which we read `upsert` (default false).
fn parse_update(collection: &str, method: &Call, chain: &[Call]) -> Result<MongoQuery, CoreError> {
    reject_terminal_only_chain(chain, method.name)?;
    let args = split_top_level_args(method.args)?;
    if args.len() < 2 || args.len() > 3 {
        return Err(CoreError::Query(format!(
            "{}() expects (filter, update, options?)",
            method.name
        )));
    }
    let filter = require_document(args[0], method.name)?;
    let update = require_document(args[1], method.name)?;
    let upsert = optional_upsert(args.get(2).copied(), method.name)?;

    let collection = collection.to_string();
    Ok(if method.name == "updateOne" {
        MongoQuery::UpdateOne {
            collection,
            filter,
            update,
            upsert,
        }
    } else {
        MongoQuery::UpdateMany {
            collection,
            filter,
            update,
            upsert,
        }
    })
}

/// `replaceOne(filter, replacement, options?)` — like `update` but the second
/// argument is a full replacement document and `upsert` comes from the options.
fn parse_replace_one(
    collection: &str,
    method: &Call,
    chain: &[Call],
) -> Result<MongoQuery, CoreError> {
    reject_terminal_only_chain(chain, "replaceOne")?;
    let args = split_top_level_args(method.args)?;
    if args.len() < 2 || args.len() > 3 {
        return Err(CoreError::Query(
            "replaceOne() expects (filter, replacement, options?)".into(),
        ));
    }
    let filter = require_document(args[0], "replaceOne")?;
    let replacement = require_document(args[1], "replaceOne")?;
    let upsert = optional_upsert(args.get(2).copied(), "replaceOne")?;
    Ok(MongoQuery::ReplaceOne {
        collection: collection.to_string(),
        filter,
        replacement,
        upsert,
    })
}

/// `deleteOne(filter)` / `deleteMany(filter)`. The filter is **required**: a bare
/// `deleteMany()` is dangerous (it would target every document), so we reject an
/// empty argument list and tell the user to pass `{}` explicitly to target all.
fn parse_delete(collection: &str, method: &Call, chain: &[Call]) -> Result<MongoQuery, CoreError> {
    reject_terminal_only_chain(chain, method.name)?;
    let args = split_top_level_args(method.args)?;
    if args.is_empty() {
        return Err(CoreError::Query(format!(
            "{}() requires a filter; pass `{{}}` explicitly to target all documents",
            method.name
        )));
    }
    if args.len() != 1 {
        return Err(CoreError::Query(format!(
            "{}() expects a single filter argument",
            method.name
        )));
    }
    let filter = require_document(args[0], method.name)?;
    let collection = collection.to_string();
    Ok(if method.name == "deleteOne" {
        MongoQuery::DeleteOne { collection, filter }
    } else {
        MongoQuery::DeleteMany { collection, filter }
    })
}

/// `drop()` — no arguments; drops the whole collection.
fn parse_drop(collection: &str, method: &Call, chain: &[Call]) -> Result<MongoQuery, CoreError> {
    reject_terminal_only_chain(chain, "drop")?;
    let args = split_top_level_args(method.args)?;
    if !args.is_empty() {
        return Err(CoreError::Query("drop() takes no arguments".into()));
    }
    Ok(MongoQuery::DropCollection {
        collection: collection.to_string(),
    })
}

// ---------------------------------------------------------------------------
// Chain handling
// ---------------------------------------------------------------------------

/// Apply a `find`/`findOne` chain (`.projection`/`.project`/`.sort`/`.skip`/
/// `.limit`, plus the no-op terminals `.toArray`/`.pretty`) onto the accumulators.
fn apply_find_chain(
    chain: &[Call],
    projection: &mut Option<Bson>,
    sort: &mut Option<Bson>,
    skip: &mut Option<u64>,
    limit: &mut Option<i64>,
) -> Result<(), CoreError> {
    for call in chain {
        match call.name {
            "projection" | "project" => {
                *projection = Some(require_doc_arg(call, call.name)?);
            }
            "sort" => {
                *sort = Some(require_doc_arg(call, "sort")?);
            }
            "skip" => {
                *skip = Some(require_u64_arg(call, "skip")?);
            }
            "limit" => {
                *limit = Some(require_i64_arg(call, "limit")?);
            }
            // Shell terminals that only affect the mongosh REPL's presentation.
            "toArray" | "pretty" => {}
            other => {
                return Err(CoreError::Query(format!(
                    "unsupported find chain method `.{other}()`"
                )))
            }
        }
    }
    Ok(())
}

/// For methods that take no meaningful chain, allow only the no-op shell
/// terminals (`.toArray()`/`.pretty()`) and reject anything else.
fn reject_terminal_only_chain(chain: &[Call], method: &str) -> Result<(), CoreError> {
    for call in chain {
        if !matches!(call.name, "toArray" | "pretty") {
            return Err(CoreError::Query(format!(
                "`.{}()` cannot be chained onto {method}()",
                call.name
            )));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Argument coercion helpers
// ---------------------------------------------------------------------------

/// An empty BSON document (`{}`) — the default filter/projection.
fn empty_doc() -> Bson {
    Bson::Document(bson::Document::new())
}

/// Parse a single JSON argument into a [`Bson`]. Strict JSON only (see module
/// docs). Errors name `method` so the message points at the offending call.
fn parse_json_arg(raw: &str, method: &str) -> Result<Bson, CoreError> {
    let trimmed = raw.trim();
    let value: serde_json::Value = serde_json::from_str(trimmed).map_err(|e| {
        CoreError::Query(format!(
            "invalid JSON argument to {method}(): {e} (arguments must be strict JSON)"
        ))
    })?;
    bson::to_bson(&value).map_err(|e| {
        CoreError::Query(format!(
            "could not convert {method}() argument to BSON: {e}"
        ))
    })
}

/// Parse an optional argument that must be a document (`{...}`) when present.
fn optional_doc_arg(raw: Option<&str>, method: &str) -> Result<Option<Bson>, CoreError> {
    match raw {
        None => Ok(None),
        Some(r) if r.trim().is_empty() => Ok(None),
        Some(r) => {
            let b = parse_json_arg(r, method)?;
            if !matches!(b, Bson::Document(_)) {
                return Err(CoreError::Query(format!(
                    "{method}() argument must be a document object `{{ … }}`"
                )));
            }
            Ok(Some(b))
        }
    }
}

/// Require a raw argument that parses to a document (`{...}`). Used for the
/// required document arguments of the write methods (filter/update/replacement/
/// inserted document).
fn require_document(raw: &str, method: &str) -> Result<Bson, CoreError> {
    let b = parse_json_arg(raw, method)?;
    if !matches!(b, Bson::Document(_)) {
        return Err(CoreError::Query(format!(
            "{method}() argument must be a document object `{{ … }}`"
        )));
    }
    Ok(b)
}

/// Parse an optional write-options document, returning its `upsert` flag
/// (default `false`). The options doc must be an object when present, and if it
/// carries an `upsert` key that key must be a boolean. Other option keys are
/// tolerated but ignored — Selene surfaces only `upsert` for now.
fn optional_upsert(raw: Option<&str>, method: &str) -> Result<bool, CoreError> {
    let Some(options) = optional_doc_arg(raw, method)? else {
        return Ok(false);
    };
    // `optional_doc_arg` guarantees a Document here.
    let Bson::Document(doc) = options else {
        return Ok(false);
    };
    match doc.get("upsert") {
        None => Ok(false),
        Some(Bson::Boolean(b)) => Ok(*b),
        Some(_) => Err(CoreError::Query(format!(
            "{method}() `upsert` option must be a boolean (`true`/`false`)"
        ))),
    }
}

/// Require a single document argument for a chain call (`.sort({...})`, …).
fn require_doc_arg(call: &Call, name: &str) -> Result<Bson, CoreError> {
    let args = split_top_level_args(call.args)?;
    if args.len() != 1 {
        return Err(CoreError::Query(format!(
            ".{name}() expects a single document argument"
        )));
    }
    let b = parse_json_arg(args[0], name)?;
    if !matches!(b, Bson::Document(_)) {
        return Err(CoreError::Query(format!(
            ".{name}() argument must be a document object `{{ … }}`"
        )));
    }
    Ok(b)
}

/// Require a single non-negative integer argument (`.skip(n)`).
fn require_u64_arg(call: &Call, name: &str) -> Result<u64, CoreError> {
    let n = require_integer_arg(call, name)?;
    u64::try_from(n)
        .map_err(|_| CoreError::Query(format!(".{name}() expects a non-negative integer")))
}

/// Require a single integer argument (`.limit(n)`; may be negative in mongosh).
fn require_i64_arg(call: &Call, name: &str) -> Result<i64, CoreError> {
    require_integer_arg(call, name)
}

/// Shared integer-argument parsing for `.skip`/`.limit`.
fn require_integer_arg(call: &Call, name: &str) -> Result<i64, CoreError> {
    let args = split_top_level_args(call.args)?;
    if args.len() != 1 {
        return Err(CoreError::Query(format!(
            ".{name}() expects a single integer argument"
        )));
    }
    match parse_json_arg(args[0], name)? {
        Bson::Int32(v) => Ok(v as i64),
        Bson::Int64(v) => Ok(v),
        // A JSON number with no fractional part arrives as Double; accept it if
        // it is integral (mongosh users often type `.limit(20)` which is fine,
        // but a literal `20.0` should also work).
        Bson::Double(d) if d.fract() == 0.0 => Ok(d as i64),
        _ => Err(CoreError::Query(format!(
            ".{name}() expects an integer argument"
        ))),
    }
}

// ---------------------------------------------------------------------------
// Stage 1: tokenizing the `db.coll.method(...)` chain
// ---------------------------------------------------------------------------

/// Split the source into its leading bare handle (`db`) and the ordered chain of
/// `.name(args)` calls that follow. Any trailing `;`/whitespace is ignored.
///
/// Only the leading segment before the first `.` is returned as the handle; the
/// collection name is a *bare* (parenless) segment handled by
/// [`split_collection`].
fn split_calls(src: &str) -> Result<(&str, Vec<Segment<'_>>), CoreError> {
    let bytes = src.as_bytes();

    // Leading handle: run of identifier bytes up to the first `.`.
    let handle_end = ident_end(bytes, 0);
    if handle_end == 0 {
        return Err(CoreError::Query(
            "query must start with the `db` handle".into(),
        ));
    }
    let handle = &src[..handle_end];
    let mut i = handle_end;

    let mut segments: Vec<Segment> = Vec::new();

    while i < bytes.len() {
        // Skip a trailing statement terminator / whitespace after the chain.
        if bytes[i].is_ascii_whitespace() || bytes[i] == b';' {
            i += 1;
            continue;
        }
        if bytes[i] != b'.' {
            return Err(CoreError::Query(format!(
                "expected `.` in the query chain at byte {i}"
            )));
        }
        i += 1; // consume the '.'

        // Segment name: run of identifier bytes.
        let name_start = i;
        let name_end = ident_end(bytes, i);
        if name_end == name_start {
            return Err(CoreError::Query(
                "expected an identifier after `.` in the query chain".into(),
            ));
        }
        let name = &src[name_start..name_end];
        i = name_end;

        // Optional call parens.
        skip_ws(bytes, &mut i);
        if i < bytes.len() && bytes[i] == b'(' {
            let (args, next) = match_parens(src, i)?;
            segments.push(Segment::Call(Call { name, args }));
            i = next;
        } else {
            // A bare segment (the collection name, e.g. `db.orders.find(...)`).
            segments.push(Segment::Bare(name));
        }
    }

    Ok((handle, segments))
}

/// A segment in the chain after `db`: either a bare collection name or a call.
#[derive(Debug, Clone)]
enum Segment<'a> {
    Bare(&'a str),
    Call(Call<'a>),
}

/// Split the segment list into the collection name (the single leading bare
/// segment) and the trailing method calls.
fn split_collection<'a>(segments: &[Segment<'a>]) -> Result<(&'a str, Vec<Call<'a>>), CoreError> {
    let mut iter = segments.iter();
    let collection = match iter.next() {
        Some(Segment::Bare(name)) => *name,
        Some(Segment::Call(c)) => {
            return Err(CoreError::Query(format!(
                "expected a collection name after `db.`, found a call `.{}()`",
                c.name
            )))
        }
        None => {
            return Err(CoreError::Query(
                "expected `db.<collection>.<method>(…)`".into(),
            ))
        }
    };

    let mut calls = Vec::new();
    for seg in iter {
        match seg {
            Segment::Call(c) => calls.push(c.clone()),
            Segment::Bare(name) => {
                // A second bare segment (e.g. `db.a.b.find()`) is not the shape
                // we accept — collections are addressed as a single segment.
                return Err(CoreError::Query(format!(
                    "unexpected `.{name}` — expected a method call, not a bare segment"
                )));
            }
        }
    }
    Ok((collection, calls))
}

/// Given `src` and the byte index of an opening `(`, return the raw text between
/// it and its matching `)` plus the index just past the `)`. Parens/brackets/
/// braces are balanced by depth-counting that skips string literals (single- and
/// double-quoted, `\`-escaped), so a `)` inside a string cannot close the call.
fn match_parens(src: &str, open: usize) -> Result<(&str, usize), CoreError> {
    let bytes = src.as_bytes();
    debug_assert_eq!(bytes[open], b'(');
    let mut depth = 0i32;
    let mut i = open;
    let inner_start = open + 1;

    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'\'' | b'"' => {
                i = skip_string(bytes, i)?;
                continue;
            }
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => {
                depth -= 1;
                if depth == 0 {
                    // Balanced: the matching `)` closes the call.
                    if b != b')' {
                        return Err(CoreError::Query(
                            "mismatched brackets in query arguments".into(),
                        ));
                    }
                    return Ok((&src[inner_start..i], i + 1));
                }
            }
            _ => {}
        }
        i += 1;
    }
    Err(CoreError::Query(
        "unterminated `(` in the query — check the argument brackets".into(),
    ))
}

/// Split a call's raw argument text on **top-level** commas (commas nested
/// inside `{}`/`[]`/`()` or inside a string literal do not split). Returns the
/// verbatim argument slices; an all-whitespace argument list yields an empty vec.
fn split_top_level_args(args: &str) -> Result<Vec<&str>, CoreError> {
    if args.trim().is_empty() {
        return Ok(Vec::new());
    }
    let bytes = args.as_bytes();
    let mut depth = 0i32;
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;

    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'\'' | b'"' => {
                i = skip_string(bytes, i)?;
                continue;
            }
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b',' if depth == 0 => {
                out.push(&args[start..i]);
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    out.push(&args[start..]);
    Ok(out)
}

/// Advance past a string literal starting at `bytes[i]` (which must be a quote),
/// honouring `\`-escapes. Returns the index just past the closing quote.
fn skip_string(bytes: &[u8], i: usize) -> Result<usize, CoreError> {
    let quote = bytes[i];
    let mut j = i + 1;
    while j < bytes.len() {
        match bytes[j] {
            b'\\' => j += 2, // skip the escaped char (and the backslash)
            c if c == quote => return Ok(j + 1),
            _ => j += 1,
        }
    }
    Err(CoreError::Query(
        "unterminated string literal in query arguments".into(),
    ))
}

/// The end index of an identifier run starting at `start` (letters, digits,
/// `_`, `$` — MongoDB collection/field names allow `$` and `.` but we treat a
/// `.` as a chain separator, so it is not part of an identifier here).
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

/// Advance `*i` past ASCII whitespace.
fn skip_ws(bytes: &[u8], i: &mut usize) {
    while *i < bytes.len() && bytes[*i].is_ascii_whitespace() {
        *i += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bson::doc;

    fn find_parts(q: &MongoQuery) -> (&str, &Bson) {
        match q {
            MongoQuery::Find {
                collection, filter, ..
            } => (collection, filter),
            other => panic!("expected Find, got {other:?}"),
        }
    }

    #[test]
    fn find_no_args_defaults_to_empty_filter() {
        let q = parse("db.orders.find()").unwrap();
        let (coll, filter) = find_parts(&q);
        assert_eq!(coll, "orders");
        assert_eq!(filter, &Bson::Document(doc! {}));
    }

    #[test]
    fn find_with_filter() {
        let q = parse(r#"db.orders.find({ "status": "open" })"#).unwrap();
        let (coll, filter) = find_parts(&q);
        assert_eq!(coll, "orders");
        assert_eq!(filter, &Bson::Document(doc! { "status": "open" }));
    }

    #[test]
    fn find_with_filter_and_projection() {
        let q = parse(r#"db.c.find({ "a": 1 }, { "a": 1, "_id": 0 })"#).unwrap();
        match q {
            MongoQuery::Find {
                filter, projection, ..
            } => {
                // serde_json → bson maps JSON integers to Int64, so the doc!
                // literals here are written as i64 to match.
                assert_eq!(filter, Bson::Document(doc! { "a": 1i64 }));
                assert_eq!(
                    projection,
                    Some(Bson::Document(doc! { "a": 1i64, "_id": 0i64 }))
                );
            }
            other => panic!("expected Find, got {other:?}"),
        }
    }

    #[test]
    fn find_chained_sort_skip_limit_projection() {
        let q = parse(
            r#"db.c.find({}).projection({ "a": 1 }).sort({ "a": -1 }).skip(5).limit(10).toArray().pretty()"#,
        )
        .unwrap();
        match q {
            MongoQuery::Find {
                projection,
                sort,
                skip,
                limit,
                ..
            } => {
                assert_eq!(projection, Some(Bson::Document(doc! { "a": 1i64 })));
                assert_eq!(sort, Some(Bson::Document(doc! { "a": -1i64 })));
                assert_eq!(skip, Some(5));
                assert_eq!(limit, Some(10));
            }
            other => panic!("expected Find, got {other:?}"),
        }
    }

    #[test]
    fn find_one_forces_limit_one() {
        let q = parse(r#"db.c.findOne({ "a": 1 })"#).unwrap();
        match q {
            MongoQuery::Find { limit, .. } => assert_eq!(limit, Some(1)),
            other => panic!("expected Find, got {other:?}"),
        }
    }

    #[test]
    fn aggregate_pipeline() {
        let q = parse(
            r#"db.c.aggregate([{ "$match": { "status": "open" } }, { "$group": { "_id": "$c" } }])"#,
        )
        .unwrap();
        match q {
            MongoQuery::Aggregate {
                collection,
                pipeline,
            } => {
                assert_eq!(collection, "c");
                assert_eq!(pipeline.len(), 2);
                assert_eq!(
                    pipeline[0],
                    Bson::Document(doc! { "$match": { "status": "open" } })
                );
            }
            other => panic!("expected Aggregate, got {other:?}"),
        }
    }

    #[test]
    fn count_documents_and_count_alias() {
        for src in [
            r#"db.c.countDocuments({ "a": 1 })"#,
            r#"db.c.count({ "a": 1 })"#,
        ] {
            match parse(src).unwrap() {
                MongoQuery::CountDocuments { collection, filter } => {
                    assert_eq!(collection, "c");
                    assert_eq!(filter, Bson::Document(doc! { "a": 1i64 }));
                }
                other => panic!("expected CountDocuments for {src}, got {other:?}"),
            }
        }
    }

    #[test]
    fn count_no_args_is_empty_filter() {
        match parse("db.c.countDocuments()").unwrap() {
            MongoQuery::CountDocuments { filter, .. } => {
                assert_eq!(filter, Bson::Document(doc! {}));
            }
            other => panic!("expected CountDocuments, got {other:?}"),
        }
    }

    #[test]
    fn distinct_field_and_optional_filter() {
        match parse(r#"db.c.distinct("status", { "region": "eu" })"#).unwrap() {
            MongoQuery::Distinct {
                collection,
                field,
                filter,
            } => {
                assert_eq!(collection, "c");
                assert_eq!(field, "status");
                assert_eq!(filter, Bson::Document(doc! { "region": "eu" }));
            }
            other => panic!("expected Distinct, got {other:?}"),
        }
    }

    #[test]
    fn distinct_requires_field() {
        let err = parse("db.c.distinct()").expect_err("field is required");
        assert!(matches!(err, CoreError::Query(_)), "got {err:?}");
    }

    #[test]
    fn malformed_json_is_query_error() {
        // Unquoted key is not strict JSON.
        let err =
            parse("db.c.find({ status: 'open' })").expect_err("relaxed JSON is rejected in v1");
        assert!(matches!(err, CoreError::Query(_)), "got {err:?}");
    }

    #[test]
    fn write_methods_parse() {
        // insertOne → InsertOne with the document.
        match parse(r#"db.c.insertOne({ "a": 1 })"#).unwrap() {
            MongoQuery::InsertOne {
                collection,
                document,
            } => {
                assert_eq!(collection, "c");
                assert_eq!(document, Bson::Document(doc! { "a": 1i64 }));
            }
            other => panic!("expected InsertOne, got {other:?}"),
        }

        // insertMany → the array elements as documents.
        match parse(r#"db.c.insertMany([{ "a": 1 }, { "b": 2 }])"#).unwrap() {
            MongoQuery::InsertMany {
                collection,
                documents,
            } => {
                assert_eq!(collection, "c");
                assert_eq!(documents.len(), 2);
                assert_eq!(documents[0], Bson::Document(doc! { "a": 1i64 }));
            }
            other => panic!("expected InsertMany, got {other:?}"),
        }

        // updateOne → filter, update, upsert defaulting to false.
        match parse(r#"db.c.updateOne({ "a": 1 }, { "$set": { "b": 2 } })"#).unwrap() {
            MongoQuery::UpdateOne {
                collection,
                filter,
                update,
                upsert,
            } => {
                assert_eq!(collection, "c");
                assert_eq!(filter, Bson::Document(doc! { "a": 1i64 }));
                assert_eq!(update, Bson::Document(doc! { "$set": { "b": 2i64 } }));
                assert!(!upsert, "upsert defaults to false");
            }
            other => panic!("expected UpdateOne, got {other:?}"),
        }

        // updateMany parses to UpdateMany.
        assert!(matches!(
            parse(r#"db.c.updateMany({}, { "$set": { "b": 2 } })"#).unwrap(),
            MongoQuery::UpdateMany { .. }
        ));

        // deleteMany with an explicit filter parses.
        match parse("db.c.deleteMany({})").unwrap() {
            MongoQuery::DeleteMany { collection, filter } => {
                assert_eq!(collection, "c");
                assert_eq!(filter, Bson::Document(doc! {}));
            }
            other => panic!("expected DeleteMany, got {other:?}"),
        }
        assert!(matches!(
            parse(r#"db.c.deleteOne({ "a": 1 })"#).unwrap(),
            MongoQuery::DeleteOne { .. }
        ));

        // replaceOne → filter + replacement.
        match parse(r#"db.c.replaceOne({ "a": 1 }, { "a": 2 })"#).unwrap() {
            MongoQuery::ReplaceOne {
                filter,
                replacement,
                upsert,
                ..
            } => {
                assert_eq!(filter, Bson::Document(doc! { "a": 1i64 }));
                assert_eq!(replacement, Bson::Document(doc! { "a": 2i64 }));
                assert!(!upsert);
            }
            other => panic!("expected ReplaceOne, got {other:?}"),
        }

        // drop() → DropCollection.
        match parse("db.c.drop()").unwrap() {
            MongoQuery::DropCollection { collection } => assert_eq!(collection, "c"),
            other => panic!("expected DropCollection, got {other:?}"),
        }
    }

    #[test]
    fn update_parses_upsert_option() {
        match parse(r#"db.c.updateOne({ "a": 1 }, { "$set": { "b": 2 } }, { "upsert": true })"#)
            .unwrap()
        {
            MongoQuery::UpdateOne { upsert, .. } => assert!(upsert, "upsert:true should parse"),
            other => panic!("expected UpdateOne, got {other:?}"),
        }
        // replaceOne likewise reads upsert from its options doc.
        match parse(r#"db.c.replaceOne({ "a": 1 }, { "a": 2 }, { "upsert": true })"#).unwrap() {
            MongoQuery::ReplaceOne { upsert, .. } => assert!(upsert),
            other => panic!("expected ReplaceOne, got {other:?}"),
        }
    }

    #[test]
    fn delete_without_filter_is_query_error() {
        // A bare deleteMany()/deleteOne() with no filter is refused (targeting
        // every document must be explicit via `{}`).
        for src in ["db.c.deleteMany()", "db.c.deleteOne()"] {
            let err = parse(src).expect_err("a filter-less delete is rejected");
            assert!(matches!(err, CoreError::Query(_)), "{src}: got {err:?}");
        }
    }

    #[test]
    fn still_unsupported_writes() {
        // Higher-level writes that Selene does not yet run stay Unsupported.
        for src in [
            r#"db.c.findOneAndUpdate({}, { "$set": { "a": 1 } })"#,
            r#"db.c.findOneAndReplace({}, {})"#,
            r#"db.c.findOneAndDelete({})"#,
            "db.c.bulkWrite([])",
            r#"db.c.createIndex({ "a": 1 })"#,
            r#"db.c.createIndexes([{ "a": 1 }])"#,
            r#"db.c.renameCollection("d")"#,
            "db.c.dropDatabase()",
        ] {
            let err = parse(src).unwrap_err();
            assert!(
                matches!(err, CoreError::Unsupported(_)),
                "expected Unsupported for {src}, got {err:?}"
            );
        }
    }

    #[test]
    fn non_db_handle_is_query_error() {
        let err = parse("foo.c.find({})").expect_err("must start with db");
        assert!(matches!(err, CoreError::Query(_)), "got {err:?}");
    }

    #[test]
    fn string_literal_with_parens_and_braces_does_not_break_matching() {
        // A `)` and `}` inside a string must not close the call early.
        let q = parse(r#"db.c.find({ "note": "a) and }" })"#).unwrap();
        let (_, filter) = find_parts(&q);
        assert_eq!(filter, &Bson::Document(doc! { "note": "a) and }" }));
    }

    #[test]
    fn double_quoted_comma_inside_string_is_not_an_arg_separator() {
        // The comma inside the string must not split find into two arguments.
        let q = parse(r#"db.c.find({ "a": "x,y" })"#).unwrap();
        let (_, filter) = find_parts(&q);
        assert_eq!(filter, &Bson::Document(doc! { "a": "x,y" }));
    }

    #[test]
    fn unknown_method_is_query_error() {
        let err = parse("db.c.frobnicate({})").expect_err("unknown method");
        assert!(matches!(err, CoreError::Query(_)), "got {err:?}");
    }

    #[test]
    fn unterminated_parens_is_query_error() {
        let err = parse("db.c.find({ \"a\": 1 }").expect_err("missing close paren");
        assert!(matches!(err, CoreError::Query(_)), "got {err:?}");
    }

    #[test]
    fn trailing_semicolon_is_ignored() {
        let q = parse("db.c.find();").unwrap();
        let (coll, _) = find_parts(&q);
        assert_eq!(coll, "c");
    }

    #[test]
    fn limit_accepts_negative() {
        // mongosh allows a negative limit (a hint); we carry it through as i64.
        match parse("db.c.find().limit(-3)").unwrap() {
            MongoQuery::Find { limit, .. } => assert_eq!(limit, Some(-3)),
            other => panic!("expected Find, got {other:?}"),
        }
    }
}
