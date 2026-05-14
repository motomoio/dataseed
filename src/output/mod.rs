//! Output formats. Each emitter streams rows to a generic `Write` so the
//! CLI doesn't care whether the sink is stdout or a file.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};

use crate::ast::{File, OutputKind};
use crate::generators::{self, Cell, Generator};
use crate::pool::GeneratedPool;
use crate::rng::SeedRng;
use crate::SemanticError;

mod json;
mod sql;

/// Per-row engine context handed to every generator's `produce`. Bundles
/// everything a generator might read at run-time:
/// * `row` — the 0-based row index within the current table.
/// * `pool` — the materialized parent-table column store for `ref()` lookups.
/// * `forced_parent` — when present, instructs a matching `ref()` call to use
///   this parent index instead of drawing uniformly. Used by per_parent
///   quota assignment (Task 1.4) and will be reused by future features.
pub struct RowCtx<'a> {
    pub row: u64,
    pub pool: &'a crate::pool::GeneratedPool,
    pub forced_parent: Option<(&'a str, &'a str, usize)>,
}

/// Single-table compatibility shim. Phase 1/2 callers (and tests) use this
/// shape; multi-table callers use [`render_plan`] instead.
pub fn render(
    file: &File,
    count: u64,
    rng: &mut SeedRng,
    out: &mut dyn Write,
) -> Result<(), RenderError> {
    assert!(
        file.tables.len() == 1 && file.generate.len() == 1,
        "render() is the single-table compat shim — use render_plan() for multi-table"
    );
    let plan = RenderPlan {
        topo_order: vec![file.tables[0].name.clone()],
        referenced: BTreeMap::new(),
        counts: {
            let mut m = BTreeMap::new();
            m.insert(file.tables[0].name.clone(), count);
            m
        },
        emit_only: None,
        per_parent_owners: BTreeMap::new(),
    };
    render_plan(file, &plan, rng, out)
}

/// Multi-table render plan. Phase 3 task 23 hands one of these to the engine
/// after computing it from the file + CLI overrides + semantic report.
#[derive(Debug, Clone)]
pub struct RenderPlan {
    /// Generation order — semantic-time topological sort.
    pub topo_order: Vec<String>,
    /// Which `(table, column)` pairs need pool retention.
    pub referenced: BTreeMap<String, BTreeSet<String>>,
    /// Per-table row counts (CLI may have overridden in-file directives).
    pub counts: BTreeMap<String, u64>,
    /// If `Some(set)`, only these tables' rows are written; others are
    /// still generated and pooled (so refs resolve) but their rows aren't
    /// emitted. Use case: `dataseed plant ... --table orders`.
    pub emit_only: Option<BTreeSet<String>>,
    /// child table → (parent table, parent column, (lo, hi)).
    /// Populated by the semantic checker; consumed here to drive per-parent
    /// quotas for child-row counts.
    pub per_parent_owners: BTreeMap<String, (String, String, (u64, u64))>,
}

pub fn render_plan(
    file: &File,
    plan: &RenderPlan,
    rng: &mut SeedRng,
    out: &mut dyn Write,
) -> Result<(), RenderError> {
    let mut pool = GeneratedPool::with_plan(plan.referenced.clone());
    let multi_table = plan.topo_order.len() > 1;

    // Compute which tables will produce visible output. Used to bookend the
    // JSON wrapping object (only opened/closed if any emit happens).
    let emitted: Vec<&str> = plan
        .topo_order
        .iter()
        .filter(|t| should_emit(plan, t))
        .map(|s| s.as_str())
        .collect();

    let want_json_wrapper = multi_table && file.output == OutputKind::Json && !emitted.is_empty();
    if want_json_wrapper {
        writeln!(out, "{{")?;
    }

    let mut emitted_count = 0usize;
    for table_name in &plan.topo_order {
        let table = file
            .table(table_name)
            .expect("plan references undeclared table");
        let emit = should_emit(plan, table_name);

        // Bind once per table.
        let mut gens: Vec<Box<dyn Generator>> = Vec::with_capacity(table.fields.len());
        for field in &table.fields {
            gens.push(generators::bind(&field.call)?);
        }

        // Compute per_parent quota assignment if this table is owned.
        // For each parent row, draw a quota k ∈ [lo, hi] inclusive, then
        // build a flat parent-index vector of length = sum(quotas) where
        // each entry is the parent row index for the child at that row.
        let owned = plan.per_parent_owners.get(table_name).cloned();
        let per_parent_assignment: Option<(String, String, Vec<usize>)> =
            owned.as_ref().map(|(parent, parent_col, (lo, hi))| {
                let parent_values = pool
                    .get(parent, parent_col)
                    .expect("parent values present in topo order");
                let parent_count = parent_values.len();
                let mut quotas: Vec<u64> = Vec::with_capacity(parent_count);
                for _ in 0..parent_count {
                    let k = if lo == hi {
                        *lo
                    } else {
                        rng.gen_range_i64(*lo as i64, *hi as i64) as u64
                    };
                    quotas.push(k);
                }
                let total: u64 = quotas.iter().sum();
                let mut v: Vec<usize> = Vec::with_capacity(total as usize);
                for (pi, &k) in quotas.iter().enumerate() {
                    for _ in 0..k {
                        v.push(pi);
                    }
                }
                (parent.clone(), parent_col.clone(), v)
            });

        let count: u64 = per_parent_assignment
            .as_ref()
            .map(|(_, _, v)| v.len() as u64)
            .unwrap_or_else(|| plan.counts.get(table_name).copied().unwrap_or(0));

        let forced = per_parent_assignment
            .as_ref()
            .map(|(t, c, v)| (t.as_str(), c.as_str(), v.as_slice()));

        if emit {
            match file.output {
                OutputKind::Sql | OutputKind::Postgis => {
                    let dialect = match file.output {
                        OutputKind::Postgis => sql::Dialect::Postgis,
                        _ => sql::Dialect::Plain,
                    };
                    if multi_table {
                        // Blank line between tables (after the first emitted).
                        if emitted_count > 0 {
                            writeln!(out)?;
                        }
                        writeln!(out, "-- Table: {} ({} rows)", table.name, count)?;
                    }
                    sql::write_sql(table, &mut gens, count, rng, out, dialect, &mut pool, forced)?;
                }
                OutputKind::Json => {
                    if multi_table {
                        let is_last = emitted_count + 1 == emitted.len();
                        write!(out, "  {}: ", serde_json::to_string(&table.name)
                            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?)?;
                        json::write_json_inline(table, &mut gens, count, rng, out, &mut pool, forced)?;
                        if !is_last {
                            writeln!(out, ",")?;
                        } else {
                            writeln!(out)?;
                        }
                    } else {
                        json::write_json(table, &mut gens, count, rng, out, &mut pool, forced)?;
                    }
                }
            }
            emitted_count += 1;
        } else {
            // Generate-but-don't-emit — still drive the row loop so refs
            // resolve. The pool gets populated as a side effect.
            drain_into_pool_with_forced(table, &mut gens, count, rng, &mut pool, forced);
        }
    }

    if want_json_wrapper {
        writeln!(out, "}}")?;
    }
    Ok(())
}

fn should_emit(plan: &RenderPlan, table: &str) -> bool {
    plan.emit_only
        .as_ref()
        .map(|set| set.contains(table))
        .unwrap_or(true)
}

/// Run `count` rows worth of generation purely for pool side effects.
/// Used when `--table NAME` filters out a dependency's emission but we
/// still need its values materialised. Honours a per-parent assignment if
/// the filtered-out table is per_parent-owned.
fn drain_into_pool_with_forced(
    table: &crate::ast::Table,
    gens: &mut [Box<dyn Generator>],
    count: u64,
    rng: &mut SeedRng,
    pool: &mut GeneratedPool,
    forced_parent_assignment: Option<(&str, &str, &[usize])>,
) {
    for row in 0..count {
        let forced_parent =
            forced_parent_assignment.map(|(t, c, assn)| (t, c, assn[row as usize]));
        // Discard the cells — we only care about the side effects on `pool`.
        let _ = produce_row(&table.name, &table.fields, gens, rng, row, pool, forced_parent);
    }
}

#[derive(Debug)]
pub enum RenderError {
    Semantic(SemanticError),
    Io(io::Error),
}

impl std::fmt::Display for RenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RenderError::Semantic(e) => write!(f, "{e}"),
            RenderError::Io(e) => write!(f, "Error: I/O failure: {e}"),
        }
    }
}

impl std::error::Error for RenderError {}
impl From<SemanticError> for RenderError {
    fn from(e: SemanticError) -> Self { RenderError::Semantic(e) }
}
impl From<io::Error> for RenderError {
    fn from(e: io::Error) -> Self { RenderError::Io(e) }
}

/// Produce one row's worth of cells AND push any referenced columns into
/// the pool so subsequent tables can `ref()` them. The two-pass shape (read
/// then update) is what lets generators take `&GeneratedPool` while the
/// engine holds the `&mut` outside.
pub(crate) fn produce_row(
    table_name: &str,
    fields: &[crate::ast::FieldDef],
    gens: &mut [Box<dyn Generator>],
    rng: &mut SeedRng,
    row: u64,
    pool: &mut GeneratedPool,
    forced_parent: Option<(&str, &str, usize)>,
) -> Vec<Cell> {
    let ctx = RowCtx { row, pool: &*pool, forced_parent };
    let cells: Vec<Cell> = gens
        .iter_mut()
        .map(|g| g.produce(rng, &ctx))
        .collect();
    for (field, cell) in fields.iter().zip(cells.iter()) {
        if pool.is_referenced(table_name, &field.name) {
            pool.push(table_name, &field.name, cell.clone());
        }
    }
    cells
}
