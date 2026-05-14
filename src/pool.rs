//! Materialized column store for `ref()` lookups.
//!
//! During multi-table generation we walk tables in topological order. As
//! each table is generated, its row values are produced field-by-field —
//! and for fields that downstream tables reference (`ref(parent.col)`),
//! we push the values into the pool. Later tables read from the pool to
//! resolve their refs.
//!
//! Only columns that are actually referenced get materialized — the
//! semantic checker computes the (table, column) target set in advance
//! and the engine asks the pool whether each field is "live" before
//! retaining its value.

use std::collections::{BTreeMap, BTreeSet};

use crate::generators::Cell;

#[derive(Debug, Default, Clone)]
pub struct GeneratedPool {
    // table_name → column_name → values
    data: BTreeMap<String, BTreeMap<String, Vec<Cell>>>,
    /// Which `(table, column)` pairs are referenced by some `ref()` call.
    /// Computed once at semantic-check time; consulted on every row to
    /// decide whether to retain the value.
    referenced: BTreeMap<String, BTreeSet<String>>,
}

impl GeneratedPool {
    /// Build a pool that will retain only the columns named in `referenced`.
    /// Pass an empty map to disable retention entirely.
    pub fn with_plan(referenced: BTreeMap<String, BTreeSet<String>>) -> Self {
        Self { data: BTreeMap::new(), referenced }
    }

    /// Has the pool plan marked `table.column` as a future `ref()` target?
    /// Used by the engine to skip materializing fields no one will read.
    pub fn is_referenced(&self, table: &str, column: &str) -> bool {
        self.referenced
            .get(table)
            .map(|cols| cols.contains(column))
            .unwrap_or(false)
    }

    /// Retain `value` for later `ref()` lookups. No-op for unreferenced
    /// columns — the engine pre-checks via `is_referenced`, this is a
    /// belt-and-braces guard.
    pub fn push(&mut self, table: &str, column: &str, value: Cell) {
        if !self.is_referenced(table, column) {
            return;
        }
        self.data
            .entry(table.to_string())
            .or_default()
            .entry(column.to_string())
            .or_default()
            .push(value);
    }

    /// Look up the materialized values for `table.column`. `None` if the
    /// table/column wasn't in the retention plan, or if no rows have been
    /// generated yet (the latter indicates an engine-ordering bug — refs
    /// to a not-yet-generated table should be impossible after topo sort).
    pub fn get(&self, table: &str, column: &str) -> Option<&[Cell]> {
        self.data
            .get(table)
            .and_then(|cols| cols.get(column))
            .map(|v| v.as_slice())
    }
}
