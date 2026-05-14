//! Semantic validation of an `ast::File`.
//!
//! Two layers:
//!   * [`catalog`] — unknown function names + per-generator arg/arity/type
//!     checks (Phase 1+2 work).
//!   * [`relations`] — Phase 3 multi-table cross-checks: ref-target
//!     existence, no self-refs, generate/table parity, topological sort,
//!     cycle detection, and the materialisation plan.
//!
//! All checks collect errors instead of stopping at the first, so a user
//! editing a `.dataseed` file sees the full diagnostic in one pass.

use std::collections::{BTreeMap, BTreeSet};

use crate::ast::File;
use crate::error::SemanticError;

pub mod catalog;
pub mod relations;

pub use catalog::suggest;

/// Combined result of semantic analysis.
///
/// * `errors` — every problem found, in encounter order.
/// * `topo_order` — generation order for the engine. Only meaningful when
///   `errors.is_empty()`; otherwise empty.
/// * `referenced` — set of `(table, column)` pairs targeted by some
///   `ref()` call. The engine uses this to decide which values to retain
///   in the pool.
#[derive(Debug, Default, Clone)]
pub struct SemanticReport {
    pub errors: Vec<SemanticError>,
    pub topo_order: Vec<String>,
    pub referenced: BTreeMap<String, BTreeSet<String>>,
    /// child table → (parent table, parent column, (lo, hi)). Populated by
    /// the relations pass when a child has exactly one `per_parent` ref —
    /// the engine reads this to derive child row counts from parent draws.
    pub per_parent_owners: BTreeMap<String, (String, String, (u64, u64))>,
}

impl SemanticReport {
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }
}

/// Validate `file` and return a full report. Always runs both check
/// passes; the relations pass is harmless when there's only one table.
pub fn check(file: &File) -> SemanticReport {
    let mut report = SemanticReport::default();

    // Catalog-level checks: unknown functions, wrong arity, type mismatch.
    catalog::check(file, &mut report.errors);

    // Relations: existence, self-refs, generate/table parity, topological
    // sort. This needs the catalog-level checks to have run first because a
    // call with the wrong shape might also mention a non-existent table —
    // we report the shape problem and skip the ref check rather than
    // double-reporting.
    let rel = relations::analyze(file);
    report.errors.extend(rel.errors);
    report.topo_order = rel.topo_order;
    report.referenced = rel.referenced;
    report.per_parent_owners = rel.per_parent_owners;

    report
}
