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
        let count = plan.counts.get(table_name).copied().unwrap_or(0);
        let emit = should_emit(plan, table_name);

        // Bind once per table.
        let mut gens: Vec<Box<dyn Generator>> = Vec::with_capacity(table.fields.len());
        for field in &table.fields {
            gens.push(generators::bind(&field.call)?);
        }

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
                    sql::write_sql(table, &mut gens, count, rng, out, dialect, &mut pool)?;
                }
                OutputKind::Json => {
                    if multi_table {
                        let is_last = emitted_count + 1 == emitted.len();
                        write!(out, "  {}: ", serde_json::to_string(&table.name)
                            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?)?;
                        json::write_json_inline(table, &mut gens, count, rng, out, &mut pool)?;
                        if !is_last {
                            writeln!(out, ",")?;
                        } else {
                            writeln!(out)?;
                        }
                    } else {
                        json::write_json(table, &mut gens, count, rng, out, &mut pool)?;
                    }
                }
            }
            emitted_count += 1;
        } else {
            // Generate-but-don't-emit — still drive the row loop so refs
            // resolve. The pool gets populated as a side effect.
            drain_into_pool(table, &mut gens, count, rng, &mut pool);
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
/// still need its values materialised.
fn drain_into_pool(
    table: &crate::ast::Table,
    gens: &mut [Box<dyn Generator>],
    count: u64,
    rng: &mut SeedRng,
    pool: &mut GeneratedPool,
) {
    for row in 0..count {
        // Discard the cells — we only care about the side effects on `pool`.
        let _ = produce_row(&table.name, &table.fields, gens, rng, row, pool);
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
) -> Vec<Cell> {
    let cells: Vec<Cell> = gens
        .iter_mut()
        .map(|g| g.produce(rng, row, &*pool))
        .collect();
    for (field, cell) in fields.iter().zip(cells.iter()) {
        if pool.is_referenced(table_name, &field.name) {
            pool.push(table_name, &field.name, cell.clone());
        }
    }
    cells
}
