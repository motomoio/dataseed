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
pub mod ddl;

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
        self_ref_tables: BTreeSet::new(),
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
    /// Names of tables that have at least one legal self-reference. The
    /// engine pre-computes target column values into the pool BEFORE
    /// generating the dependent rows, so per-row refs to the same table
    /// resolve uniformly across ALL rows (including rows generated later
    /// in the same run).
    pub self_ref_tables: BTreeSet<String>,
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

        // Self-reference pre-pass (Phase 4.2): for any table in
        // `plan.self_ref_tables`, pre-compute every row's value for the
        // columns that are referenced by another field IN THE SAME table,
        // and push those values into the pool BEFORE running the row-emit
        // loop. The row-emit loop then reuses the cached cell for the
        // target field (so RNG order is preserved) and lets the self-ref
        // field find a fully-populated pool when it draws.
        //
        // Determinism contract: RNG draws for cached target fields happen
        // ONCE (here), in field-declaration order, for all rows. The row
        // emission loop skips re-generating those target fields and uses
        // the cached value. Non-target fields (including the self-refs
        // themselves) still consume RNG once per row in the emission loop.
        let self_ref_active = plan.self_ref_tables.contains(table_name);
        let target_field_indices: BTreeSet<usize> = if self_ref_active {
            let mut targets = BTreeSet::new();
            for (i, field) in table.fields.iter().enumerate() {
                let referenced_by_other =
                    table.fields.iter().enumerate().any(|(j, other)| {
                        j != i && call_refs_self_column(&other.call, table_name, &field.name)
                    });
                if referenced_by_other {
                    targets.insert(i);
                }
            }
            targets
        } else {
            BTreeSet::new()
        };

        // Compute per_parent quota assignment if this table is owned.
        // For each parent row, draw a quota k ∈ [lo, hi] inclusive, then
        // build a flat parent-index vector of length = sum(quotas) where
        // each entry is the parent row index for the child at that row.
        //
        // Determinism contract: RNG draws per table happen in two phases.
        //   (1) Per-parent quota draws (one per parent row, only when this child
        //       table is per_parent-owned). Skipped entirely otherwise.
        //   (2) Per-row generator draws (one batch per child row).
        // Reordering phases or interleaving them WILL change the byte stream
        // for any seed — see the SHA table in README for the contract this
        // satisfies.
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

        // Pre-pass: generate the target fields for every row and push them
        // into the pool. Cache the resulting cells by [row][field] so the
        // row-emit loop can reuse them without re-draining the RNG.
        let cached_cells: Vec<Vec<Option<Cell>>> =
            if self_ref_active && !target_field_indices.is_empty() {
                let mut cache: Vec<Vec<Option<Cell>>> =
                    vec![vec![None; table.fields.len()]; count as usize];
                for r in 0..count {
                    for &i in &target_field_indices {
                        let ctx = RowCtx { row: r, pool: &pool, forced_parent: None };
                        let cell = gens[i].produce(rng, &ctx);
                        if pool.is_referenced(&table.name, &table.fields[i].name) {
                            pool.push(&table.name, &table.fields[i].name, cell.clone());
                        }
                        cache[r as usize][i] = Some(cell);
                    }
                }
                cache
            } else {
                Vec::new()
            };
        let cached_cells_slice: Option<&[Vec<Option<Cell>>]> =
            if cached_cells.is_empty() { None } else { Some(cached_cells.as_slice()) };

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
                    sql::write_sql(table, &mut gens, count, rng, out, dialect, &mut pool, forced, cached_cells_slice)?;
                }
                OutputKind::Json => {
                    if multi_table {
                        let is_last = emitted_count + 1 == emitted.len();
                        write!(out, "  {}: ", serde_json::to_string(&table.name)
                            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?)?;
                        json::write_json_inline(table, &mut gens, count, rng, out, &mut pool, forced, cached_cells_slice)?;
                        if !is_last {
                            writeln!(out, ",")?;
                        } else {
                            writeln!(out)?;
                        }
                    } else {
                        json::write_json(table, &mut gens, count, rng, out, &mut pool, forced, cached_cells_slice)?;
                    }
                }
            }
            emitted_count += 1;
        } else {
            // Generate-but-don't-emit — still drive the row loop so refs
            // resolve. The pool gets populated as a side effect.
            drain_into_pool(table, &mut gens, count, rng, &mut pool, forced, cached_cells_slice);
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
fn drain_into_pool(
    table: &crate::ast::Table,
    gens: &mut [Box<dyn Generator>],
    count: u64,
    rng: &mut SeedRng,
    pool: &mut GeneratedPool,
    forced_parent_assignment: Option<(&str, &str, &[usize])>,
    cached_target_cells: Option<&[Vec<Option<Cell>>]>,
) {
    for row in 0..count {
        let forced_parent =
            forced_parent_assignment.map(|(t, c, assn)| (t, c, assn[row as usize]));
        let row_cache = cached_target_cells.map(|c| c[row as usize].as_slice());
        // Discard the cells — we only care about the side effects on `pool`.
        let _ = produce_row(
            &table.name,
            &table.fields,
            gens,
            rng,
            row,
            pool,
            forced_parent,
            row_cache,
        );
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
///
/// `cached_target_cells` — when self-ref two-pass is active for this
/// table, each entry is either `Some(cell)` for a pre-computed target
/// field (reuse the cached value, don't re-draw the RNG) or `None` for a
/// field that should be generated normally. Caller passes `None` for
/// non-self-ref tables, which preserves the original per-row shape.
pub(crate) fn produce_row(
    table_name: &str,
    fields: &[crate::ast::FieldDef],
    gens: &mut [Box<dyn Generator>],
    rng: &mut SeedRng,
    row: u64,
    pool: &mut GeneratedPool,
    forced_parent: Option<(&str, &str, usize)>,
    cached_target_cells: Option<&[Option<Cell>]>,
) -> Vec<Cell> {
    let ctx = RowCtx { row, pool: &*pool, forced_parent };
    let cells: Vec<Cell> = gens
        .iter_mut()
        .enumerate()
        .map(|(i, g)| {
            if let Some(cache) = cached_target_cells {
                if let Some(c) = &cache[i] {
                    return c.clone();
                }
            }
            g.produce(rng, &ctx)
        })
        .collect();
    for (i, (field, cell)) in fields.iter().zip(cells.iter()).enumerate() {
        // Cached cells were already pushed in the pre-pass — don't
        // double-push or the pool will hold duplicates.
        let from_cache = cached_target_cells.map_or(false, |c| c[i].is_some());
        if !from_cache && pool.is_referenced(table_name, &field.name) {
            pool.push(table_name, &field.name, cell.clone());
        }
    }
    cells
}

/// `true` iff `call` contains a `Value::ColumnRef { table, column }` with
/// the given table and column anywhere (positional, kwargs, nested arrays).
/// Used to find self-ref TARGET fields when computing the pre-pass set.
fn call_refs_self_column(
    call: &crate::ast::Call,
    table: &str,
    column: &str,
) -> bool {
    fn walk(v: &crate::ast::Value, t: &str, c: &str) -> bool {
        match v {
            crate::ast::Value::ColumnRef { table, column } => table == t && column == c,
            crate::ast::Value::Array(items) => items.iter().any(|it| walk(it, t, c)),
            _ => false,
        }
    }
    call.positional.iter().any(|v| walk(v, table, column))
        || call.kwargs.iter().any(|(_, v)| walk(v, table, column))
}
