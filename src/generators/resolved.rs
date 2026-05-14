//! Generator-argument values that may be either fixed literals (decided at
//! bind time) or per-row pool lookups (resolved during `produce`).
//!
//! This is the mechanism for refs-as-arguments — e.g.
//! `randomPointNear(center: ref(warehouses.location), radius_m: 1000)`.
//! Without it, every generator's bind function would have to special-case
//! "is this arg literal or a ref?" — instead, generators take a
//! `Resolved<T>` and call `.resolve(rng, ctx)` per row.

use crate::generators::Cell;
use crate::generators::distribution::Distribution;
use crate::output::RowCtx;
use crate::rng::SeedRng;

#[derive(Debug, Clone)]
pub enum Resolved<T: Clone> {
    /// Fixed value, decided at bind time. No per-row RNG draw, no pool lookup.
    Literal(T),

    /// Per-row pool lookup. Each call to `resolve` consumes one RNG draw
    /// (the parent-index pick via the given distribution) and one pool read.
    Ref {
        table: String,
        column: String,
        distribution: Distribution,
        /// How to convert a parent `Cell` into the target type. Returns
        /// `None` if the parent column's runtime type doesn't match — the
        /// caller decides what to do (typically panic with a clear message
        /// since this should be caught at semantic-check time).
        cast: fn(&Cell) -> Option<T>,
    },
}

impl<T: Clone> Resolved<T> {
    /// Produce the concrete value for the current row.
    ///
    /// For `Literal`, no RNG or pool work.
    /// For `Ref`, draws one parent index via `distribution.draw` and reads
    /// one cell from the pool. Returns `None` only if the parent pool is
    /// empty OR if `cast` rejects the cell — both are engine-time bugs that
    /// the caller should turn into panics.
    pub fn resolve(&self, rng: &mut SeedRng, ctx: &RowCtx) -> Option<T> {
        match self {
            Resolved::Literal(v) => Some(v.clone()),
            Resolved::Ref { table, column, distribution, cast } => {
                let values = ctx.pool.get(table, column)?;
                if values.is_empty() {
                    return None;
                }
                // Honour the engine-forced parent if it targets the same parent
                // *table* we do. The forced_parent's `parent_idx` is a row
                // index into that parent table, so it's valid for any of the
                // parent's materialized columns — that's what couples
                // per_parent-driven child rows to nested refs reading sibling
                // columns of the same parent. Example: a sensor with quota-
                // assigned warehouse N (via ref(warehouses.id, per_parent))
                // must also sample its `randomPointNear(center: ref(
                // warehouses.location))` near warehouse N — same parent row,
                // different column.
                let idx = match ctx.forced_parent {
                    Some((pt, _pc, parent_idx)) if pt == table.as_str() => {
                        parent_idx % values.len()
                    }
                    _ => distribution.draw(rng, values.len()),
                };
                cast(&values[idx])
            }
        }
    }
}

/// Cast helper: extract `(lon, lat)` from a `Cell::Geometry(Point { .. })`.
/// Used by `randomPointNear` (Task 3.2) when its `center` is a ref.
pub fn cast_point(c: &Cell) -> Option<(f64, f64)> {
    match c {
        Cell::Geometry(crate::geometry::Geometry::Point { lon, lat }) => Some((*lon, *lat)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Geometry;
    use crate::pool::GeneratedPool;
    use crate::rng::SeedRng;
    use std::collections::{BTreeMap, BTreeSet};

    fn empty_ctx<'a>(pool: &'a GeneratedPool) -> RowCtx<'a> {
        RowCtx { row: 0, pool, forced_parent: None }
    }

    #[test]
    fn literal_resolves_to_self_without_rng() {
        let r: Resolved<f64> = Resolved::Literal(2.5);
        let pool = GeneratedPool::with_plan(BTreeMap::new());
        let mut rng = SeedRng::from_seed(1);
        let ctx = empty_ctx(&pool);
        assert_eq!(r.resolve(&mut rng, &ctx), Some(2.5));
        // Same RNG used to draw an unrelated value — confirms `Literal` did
        // not consume any RNG state.
        let drawn = rng.pick_index(1000);
        let mut rng2 = SeedRng::from_seed(1);
        let expected = rng2.pick_index(1000);
        assert_eq!(drawn, expected, "Literal must not consume RNG");
    }

    #[test]
    fn ref_resolves_from_pool() {
        let mut referenced: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        referenced
            .entry("warehouses".into())
            .or_default()
            .insert("location".into());
        let mut pool = GeneratedPool::with_plan(referenced);
        pool.push("warehouses", "location", Cell::Geometry(Geometry::Point { lon: 1.0, lat: 2.0 }));
        pool.push("warehouses", "location", Cell::Geometry(Geometry::Point { lon: 10.0, lat: 20.0 }));

        let r: Resolved<(f64, f64)> = Resolved::Ref {
            table: "warehouses".into(),
            column: "location".into(),
            distribution: Distribution::Uniform,
            cast: cast_point,
        };
        let mut rng = SeedRng::from_seed(1);
        let ctx = empty_ctx(&pool);
        let (lon, lat) = r.resolve(&mut rng, &ctx).expect("must resolve");
        // Uniform draw of len-2 yields index 0 or 1 — both legal.
        assert!((lon, lat) == (1.0, 2.0) || (lon, lat) == (10.0, 20.0));
    }

    #[test]
    fn ref_returns_none_for_empty_pool() {
        let pool = GeneratedPool::with_plan(BTreeMap::new());
        let r: Resolved<(f64, f64)> = Resolved::Ref {
            table: "missing".into(),
            column: "x".into(),
            distribution: Distribution::Uniform,
            cast: cast_point,
        };
        let mut rng = SeedRng::from_seed(1);
        let ctx = empty_ctx(&pool);
        assert!(r.resolve(&mut rng, &ctx).is_none());
    }

    #[test]
    fn ref_honours_forced_parent_when_target_matches() {
        let mut referenced: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        referenced.entry("w".into()).or_default().insert("loc".into());
        let mut pool = GeneratedPool::with_plan(referenced);
        pool.push("w", "loc", Cell::Geometry(Geometry::Point { lon: 1.0, lat: 2.0 }));
        pool.push("w", "loc", Cell::Geometry(Geometry::Point { lon: 10.0, lat: 20.0 }));
        pool.push("w", "loc", Cell::Geometry(Geometry::Point { lon: 100.0, lat: 60.0 }));

        let r: Resolved<(f64, f64)> = Resolved::Ref {
            table: "w".into(),
            column: "loc".into(),
            distribution: Distribution::Uniform,
            cast: cast_point,
        };
        let mut rng = SeedRng::from_seed(99);
        // forced_parent points at this Ref's (table, column) — must use index 2.
        let ctx = RowCtx { row: 0, pool: &pool, forced_parent: Some(("w", "loc", 2)) };
        assert_eq!(r.resolve(&mut rng, &ctx), Some((100.0, 60.0)));
    }

    #[test]
    fn ref_ignores_forced_parent_for_mismatched_target() {
        let mut referenced: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        referenced.entry("w".into()).or_default().insert("loc".into());
        let mut pool = GeneratedPool::with_plan(referenced);
        pool.push("w", "loc", Cell::Geometry(Geometry::Point { lon: 1.0, lat: 2.0 }));

        let r: Resolved<(f64, f64)> = Resolved::Ref {
            table: "w".into(),
            column: "loc".into(),
            distribution: Distribution::Uniform,
            cast: cast_point,
        };
        let mut rng = SeedRng::from_seed(1);
        // forced_parent targets a DIFFERENT (table, column) — Resolved::Ref must
        // ignore it and draw via the distribution.
        let ctx = RowCtx { row: 0, pool: &pool, forced_parent: Some(("other", "col", 999)) };
        // With one element, the draw must succeed and return the only point.
        assert_eq!(r.resolve(&mut rng, &ctx), Some((1.0, 2.0)));
    }

    #[test]
    fn cast_point_rejects_non_point_cells() {
        assert!(cast_point(&Cell::Integer(1)).is_none());
        assert!(cast_point(&Cell::Text("oops".into())).is_none());
        assert_eq!(
            cast_point(&Cell::Geometry(Geometry::Point { lon: 1.0, lat: 2.0 })),
            Some((1.0, 2.0))
        );
    }
}
