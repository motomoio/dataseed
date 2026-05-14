//! Parsed representation of a `.dataseed` file.
//!
//! Produced by `parser::parse`; consumed by the generator + output stages.

use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct File {
    pub output: OutputKind,
    /// One or more table declarations. Phase 1/2 files have exactly one;
    /// Phase 3 introduced multi-table support. Order matches the source
    /// file (semantic-time topological sort decides generation order).
    pub tables: Vec<Table>,
    /// One `generate` directive per declared table.
    pub generate: Vec<Generate>,
}

impl File {
    /// Look up a table by name. Returns `None` if undeclared; this is a
    /// semantic-time error path, so callers should pre-validate before
    /// they reach the engine.
    pub fn table(&self, name: &str) -> Option<&Table> {
        self.tables.iter().find(|t| t.name == name)
    }

    /// Effective row count for a named table. Returns `None` if the table
    /// has no `generate` directive — also a semantic-time error.
    pub fn count_for(&self, table: &str) -> Option<u64> {
        self.generate.iter().find(|g| g.table == table).map(|g| g.count)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Table {
    pub name: String,
    pub fields: Vec<FieldDef>,
    /// Source location of the table declaration (header line). Used by
    /// duplicate-name diagnostics and lint output.
    pub line: usize,
    pub col: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Generate {
    pub table: String,
    pub count: u64,
    pub line: usize,
    pub col: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputKind {
    Sql,
    Json,
    /// PostGIS-flavored SQL. Same shape as `Sql` for scalars; geometry
    /// values are emitted as `ST_GeomFromText('...', 4326)`.
    Postgis,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FieldDef {
    pub name: String,
    pub call: Call,
    /// Source position of the field name (1-based). Used in error messages.
    pub line: usize,
    pub col: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Call {
    pub function: String,
    pub positional: Vec<Value>,
    pub kwargs: Vec<(String, Value)>,
    pub line: usize,
    pub col: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Value {
    Number(f64),
    String(String),
    Bool(bool),
    Array(Vec<Value>),
    /// `ident.ident` — used as the argument to `ref()`. Only that generator
    /// accepts this variant; every other generator surfaces a type-mismatch
    /// error when a `ColumnRef` is passed.
    ColumnRef { table: String, column: String },
    /// `N..M` integer range literal (inclusive bounds, `lo <= hi` enforced
    /// at parse time). Consumed by `ref()`'s `per_parent` kwarg; other
    /// generators surface a type-mismatch error when a `Range` is passed.
    Range { lo: i64, hi: i64 },
}

impl Value {
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Number(_) => "number",
            Value::String(_) => "string",
            Value::Bool(_) => "boolean",
            Value::Array(_) => "array",
            Value::ColumnRef { .. } => "column_reference",
            Value::Range { .. } => "range",
        }
    }
}
