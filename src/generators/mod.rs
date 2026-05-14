//! Generator runtime: the typed `Cell` values rows are made of, the
//! `Generator` trait, and the dispatcher that turns an `ast::Call` into a
//! ready-to-run bound generator.
//!
//! Argument validation lives next to dispatch (in `impls`) rather than as a
//! generic interpreter because each generator has small, distinct rules and
//! hand-rolled checks produce the clearest error messages.

use crate::ast::Call;
use crate::error::SemanticError;
use crate::rng::SeedRng;

pub mod spec;
pub mod distribution;
pub mod resolved;
mod geo;
mod impls;

#[cfg(test)]
mod tests;

pub use spec::{ArgSpec, ArgType, FunctionSpec, CATALOG};

/// Runtime row value. Distinct from `ast::Value` because:
/// * SQL output needs to distinguish integer vs real;
/// * future phases will add `Cell::Null` without touching the parser.
#[derive(Debug, Clone, PartialEq)]
pub enum Cell {
    Integer(i64),
    Real(f64),
    Text(String),
    Bool(bool),
    Geometry(crate::geometry::Geometry),
}

impl Cell {
    pub fn type_name(&self) -> &'static str {
        match self {
            Cell::Integer(_) => "integer",
            Cell::Real(_) => "real",
            Cell::Text(_) => "text",
            Cell::Bool(_) => "boolean",
            Cell::Geometry(_) => "geometry",
        }
    }
}

/// A bound, ready-to-produce generator. One instance per schema field.
///
/// Phase 3 added the `pool` parameter so `ref(table.column)` can draw from
/// materialized parent-table values. Phase 4 (Task 1.4) replaced the loose
/// `(row, pool)` pair with a single [`crate::output::RowCtx`] struct that
/// also carries an optional `forced_parent` hint for per_parent quota
/// assignment. Most generators ignore everything but the RNG.
pub trait Generator: Send {
    fn produce(&mut self, rng: &mut SeedRng, ctx: &crate::output::RowCtx) -> Cell;
}

/// Resolve an AST call into a bound generator, validating signatures along
/// the way. Unknown function names are surfaced by the semantic checker, not
/// here — callers should run that pass first if they want did-you-mean
/// hints. `bind` still returns `UnknownFunction` (without a suggestion) if
/// asked to dispatch a name it doesn't know, so misuse can't be silent.
pub fn bind(call: &Call) -> Result<Box<dyn Generator>, SemanticError> {
    impls::bind(call)
}
