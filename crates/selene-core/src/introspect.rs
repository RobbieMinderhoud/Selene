//! Schema-introspection data types.
//!
//! Introspection is **lazy per level** so a database with thousands of tables
//! doesn't have to be loaded all at once: the object tree fetches databases,
//! then schemas, then tables, then columns as nodes expand. The corresponding
//! methods live on [`Connection`](crate::Connection).

use serde::{Deserialize, Serialize};

/// A database on the server.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DatabaseInfo {
    pub name: String,
    /// System database (e.g. `master`, `tempdb`) — the UI may collapse these.
    pub is_system: bool,
    /// Availability state, e.g. `"ONLINE"`, `"OFFLINE"`. Drivers without a
    /// notion of database state report `"ONLINE"`.
    pub state_desc: String,
}

/// A schema (namespace) within a database.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaInfo {
    pub name: String,
}

/// Whether a relation is a base table or a view.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TableKind {
    Table,
    View,
}

/// A table or view within a schema.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableInfo {
    pub schema: String,
    pub name: String,
    pub kind: TableKind,
}

/// A column of a table or view.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnInfo {
    pub name: String,
    /// 1-based ordinal position as reported by the catalog.
    pub ordinal: i32,
    /// Backend type name (e.g. `"nvarchar"`).
    pub data_type: String,
    pub nullable: bool,
    pub is_primary_key: bool,
    /// Declared maximum length where applicable.
    pub max_length: Option<i32>,
}
