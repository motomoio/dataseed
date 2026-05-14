//! Unit tests for the generator catalog + dispatcher. End-to-end determinism
//! is covered separately in tests/determinism_tests.rs.

use std::collections::HashSet;
use std::sync::LazyLock;

use crate::ast::{Call, Value};
use crate::generators::{bind, spec, Cell};
use crate::output::RowCtx;
use crate::pool::GeneratedPool;
use crate::rng::SeedRng;

/// Shared empty pool — none of the existing generators read from it, so
/// every test can borrow a single static instance instead of constructing
/// a fresh one per call.
static POOL: LazyLock<GeneratedPool> = LazyLock::new(GeneratedPool::default);

/// Build a fresh `RowCtx` for a given row index with no forced parent and
/// borrowing the shared empty pool. Centralises the ctx construction so
/// tests stay terse after the trait signature change.
fn ctx(row: u64) -> RowCtx<'static> {
    RowCtx { row, pool: &POOL, forced_parent: None }
}

fn mk_call(name: &str, positional: Vec<Value>, kwargs: Vec<(String, Value)>) -> Call {
    Call { function: name.to_string(), positional, kwargs, line: 1, col: 1 }
}

/// `Box<dyn Generator>` doesn't implement `Debug`, so we can't use the
/// stdlib `unwrap_err`. This helper does the same thing without that bound.
fn expect_err(r: Result<Box<dyn crate::generators::Generator>, crate::error::SemanticError>)
    -> crate::error::SemanticError
{
    match r {
        Ok(_) => panic!("expected bind to fail"),
        Err(e) => e,
    }
}

#[test]
fn spec_and_dispatch_cover_same_names() {
    let spec_names: HashSet<&str> = spec::function_names().collect();
    let dispatch_names: HashSet<&str> =
        super::impls::dispatch_names().iter().copied().collect();
    assert_eq!(
        spec_names, dispatch_names,
        "static catalog (spec.rs) and dispatcher (impls.rs) must list the same generators"
    );
}

#[test]
fn sequence_starts_at_one_by_default() {
    let mut g = bind(&mk_call("sequence", vec![], vec![])).unwrap();
    let mut rng = SeedRng::from_seed(0);
    assert_eq!(g.produce(&mut rng, &ctx(0)), Cell::Integer(1));
    assert_eq!(g.produce(&mut rng, &ctx(1)), Cell::Integer(2));
    assert_eq!(g.produce(&mut rng, &ctx(9)), Cell::Integer(10));
}

#[test]
fn sequence_honors_start_kwarg() {
    let call = mk_call("sequence", vec![], vec![("start".into(), Value::Number(1000.0))]);
    let mut g = bind(&call).unwrap();
    let mut rng = SeedRng::from_seed(0);
    assert_eq!(g.produce(&mut rng, &ctx(0)), Cell::Integer(1000));
}

#[test]
fn random_int_respects_bounds() {
    let call = mk_call(
        "randomInt",
        vec![Value::Number(1.0), Value::Number(10.0)],
        vec![],
    );
    let mut g = bind(&call).unwrap();
    let mut rng = SeedRng::from_seed(42);
    for row in 0..200 {
        let cell = g.produce(&mut rng, &ctx(row));
        match cell {
            Cell::Integer(n) => assert!((1..=10).contains(&n), "out of bounds: {n}"),
            other => panic!("expected Integer, got {other:?}"),
        }
    }
}

#[test]
fn random_int_rejects_inverted_range() {
    let call = mk_call(
        "randomInt",
        vec![Value::Number(10.0), Value::Number(1.0)],
        vec![],
    );
    let err = expect_err(bind(&call));
    assert!(
        err.to_string().contains("min (10) must be <= max (1)"),
        "got: {err}"
    );
}

#[test]
fn random_int_rejects_non_integer() {
    let call = mk_call(
        "randomInt",
        vec![Value::Number(1.5), Value::Number(10.0)],
        vec![],
    );
    let err = expect_err(bind(&call));
    assert!(err.to_string().contains("expects integer"), "got: {err}");
}

#[test]
fn random_real_rounds_to_decimals() {
    let call = mk_call(
        "randomRealNumber",
        vec![Value::Number(0.0), Value::Number(1.0)],
        vec![("decimals".into(), Value::Number(3.0))],
    );
    let mut g = bind(&call).unwrap();
    let mut rng = SeedRng::from_seed(1);
    for _ in 0..50 {
        match g.produce(&mut rng, &ctx(0)) {
            Cell::Real(n) => {
                let scaled = n * 1000.0;
                assert!((scaled - scaled.round()).abs() < 1e-6, "not rounded to 3dp: {n}");
            }
            other => panic!("expected Real, got {other:?}"),
        }
    }
}

#[test]
fn random_bool_weight_zero_always_false() {
    let call = mk_call(
        "randomBool",
        vec![],
        vec![("weight".into(), Value::Number(0.0))],
    );
    let mut g = bind(&call).unwrap();
    let mut rng = SeedRng::from_seed(7);
    for _ in 0..100 {
        assert_eq!(g.produce(&mut rng, &ctx(0)), Cell::Bool(false));
    }
}

#[test]
fn random_bool_weight_one_always_true() {
    let call = mk_call(
        "randomBool",
        vec![],
        vec![("weight".into(), Value::Number(1.0))],
    );
    let mut g = bind(&call).unwrap();
    let mut rng = SeedRng::from_seed(7);
    for _ in 0..100 {
        assert_eq!(g.produce(&mut rng, &ctx(0)), Cell::Bool(true));
    }
}

#[test]
fn random_choice_requires_at_least_one_value() {
    let err = expect_err(bind(&mk_call("randomChoice", vec![], vec![])));
    assert!(err.to_string().contains("at least 1"), "got: {err}");
}

#[test]
fn random_choice_returns_one_of_the_choices() {
    let call = mk_call(
        "randomChoice",
        vec![
            Value::String("oak".into()),
            Value::String("birch".into()),
            Value::String("pine".into()),
        ],
        vec![],
    );
    let mut g = bind(&call).unwrap();
    let mut rng = SeedRng::from_seed(99);
    for _ in 0..50 {
        match g.produce(&mut rng, &ctx(0)) {
            Cell::Text(s) => assert!(matches!(s.as_str(), "oak" | "birch" | "pine")),
            other => panic!("expected Text, got {other:?}"),
        }
    }
}

#[test]
fn random_date_within_range() {
    let call = mk_call(
        "randomDate",
        vec![
            Value::String("2020-01-01".into()),
            Value::String("2020-12-31".into()),
        ],
        vec![],
    );
    let mut g = bind(&call).unwrap();
    let mut rng = SeedRng::from_seed(3);
    for _ in 0..50 {
        match g.produce(&mut rng, &ctx(0)) {
            Cell::Text(s) => {
                assert!(s.starts_with("2020-"), "out of range: {s}");
                assert_eq!(s.len(), 10);
            }
            other => panic!("expected Text, got {other:?}"),
        }
    }
}

#[test]
fn random_date_rejects_inverted_range() {
    let call = mk_call(
        "randomDate",
        vec![
            Value::String("2024-12-31".into()),
            Value::String("1990-01-01".into()),
        ],
        vec![],
    );
    let err = expect_err(bind(&call));
    assert!(err.to_string().contains("must be <="), "got: {err}");
}

#[test]
fn random_date_rejects_malformed() {
    let call = mk_call(
        "randomDate",
        vec![
            Value::String("yesterday".into()),
            Value::String("today".into()),
        ],
        vec![],
    );
    let err = expect_err(bind(&call));
    assert!(err.to_string().contains("ISO date"), "got: {err}");
}

#[test]
fn random_uuid_emits_valid_v4() {
    let mut g = bind(&mk_call("randomUuid", vec![], vec![])).unwrap();
    let mut rng = SeedRng::from_seed(0);
    for _ in 0..20 {
        match g.produce(&mut rng, &ctx(0)) {
            Cell::Text(s) => {
                let parsed = uuid::Uuid::parse_str(&s).expect("valid uuid string");
                assert_eq!(parsed.get_version_num(), 4, "must be v4");
            }
            other => panic!("expected Text, got {other:?}"),
        }
    }
}

// ---------- Phase 2: geospatial generators --------------------------------

use crate::geometry::Geometry;

fn array(xs: &[f64]) -> Value {
    Value::Array(xs.iter().map(|x| Value::Number(*x)).collect())
}

fn produce_geom(call: Call, seed: u64) -> Geometry {
    let mut g = bind(&call).expect("bind ok");
    let mut rng = SeedRng::from_seed(seed);
    match g.produce(&mut rng, &ctx(0)) {
        Cell::Geometry(geom) => geom,
        other => panic!("expected Geometry cell, got {other:?}"),
    }
}

#[test]
fn random_point_lies_inside_bbox() {
    let call = mk_call(
        "randomPoint",
        vec![],
        vec![("bbox".into(), array(&[3.3, 50.7, 7.2, 53.5]))],
    );
    let mut g = bind(&call).unwrap();
    let mut rng = SeedRng::from_seed(42);
    for _ in 0..200 {
        match g.produce(&mut rng, &ctx(0)) {
            Cell::Geometry(Geometry::Point { lon, lat }) => {
                assert!((3.3..=7.2).contains(&lon), "lon out of bbox: {lon}");
                assert!((50.7..=53.5).contains(&lat), "lat out of bbox: {lat}");
            }
            other => panic!("expected Point, got {other:?}"),
        }
    }
}

#[test]
fn random_point_rejects_inverted_bbox() {
    let call = mk_call(
        "randomPoint",
        vec![],
        vec![("bbox".into(), array(&[10.0, 50.0, 5.0, 60.0]))],
    );
    let err = expect_err(bind(&call));
    assert!(err.to_string().contains("minLon"), "got: {err}");
}

#[test]
fn random_point_rejects_lat_out_of_range() {
    let call = mk_call(
        "randomPoint",
        vec![],
        vec![("bbox".into(), array(&[0.0, -100.0, 1.0, 50.0]))],
    );
    let err = expect_err(bind(&call));
    assert!(err.to_string().contains("latitudes"), "got: {err}");
}

#[test]
fn random_point_rejects_bbox_wrong_length() {
    let call = mk_call(
        "randomPoint",
        vec![],
        vec![("bbox".into(), array(&[3.3, 50.7, 7.2]))],
    );
    let err = expect_err(bind(&call));
    assert!(err.to_string().contains("length 4"), "got: {err}");
}

#[test]
fn random_point_near_stays_within_radius() {
    // 1000m radius around Amsterdam.
    let call = mk_call(
        "randomPointNear",
        vec![],
        vec![
            ("center".into(), array(&[4.9, 52.37])),
            ("radius_m".into(), Value::Number(1000.0)),
        ],
    );
    let mut g = bind(&call).unwrap();
    let mut rng = SeedRng::from_seed(1);
    // Equirectangular approximation — at 52° lat, 1° lat ≈ 111.32 km,
    // 1° lon ≈ 68.5 km. 1000 m → ~0.009° lat, ~0.0146° lon. Check the
    // bounding box (Chebyshev) — we sample inside a disk, so points must
    // stay inside the disk's bounding square.
    for _ in 0..100 {
        match g.produce(&mut rng, &ctx(0)) {
            Cell::Geometry(Geometry::Point { lon, lat }) => {
                let dlat = (lat - 52.37).abs();
                let dlon = (lon - 4.9).abs();
                assert!(dlat <= 0.01, "lat off by {dlat}");
                assert!(dlon <= 0.02, "lon off by {dlon}");
            }
            other => panic!("expected Point, got {other:?}"),
        }
    }
}

#[test]
fn random_point_near_rejects_zero_radius() {
    let call = mk_call(
        "randomPointNear",
        vec![],
        vec![
            ("center".into(), array(&[0.0, 0.0])),
            ("radius_m".into(), Value::Number(0.0)),
        ],
    );
    let err = expect_err(bind(&call));
    assert!(err.to_string().contains("must be positive"), "got: {err}");
}

#[test]
fn random_line_string_has_correct_vertex_count() {
    let call = mk_call(
        "randomLineString",
        vec![],
        vec![
            ("bbox".into(), array(&[0.0, 0.0, 10.0, 10.0])),
            ("segments".into(), Value::Number(7.0)),
        ],
    );
    let g = produce_geom(call, 42);
    match g {
        Geometry::LineString(verts) => assert_eq!(verts.len(), 8, "segments+1 vertices"),
        other => panic!("expected LineString, got {other:?}"),
    }
}

#[test]
fn random_line_string_rejects_zero_segments() {
    let call = mk_call(
        "randomLineString",
        vec![],
        vec![
            ("bbox".into(), array(&[0.0, 0.0, 10.0, 10.0])),
            ("segments".into(), Value::Number(0.0)),
        ],
    );
    let err = expect_err(bind(&call));
    assert!(err.to_string().contains(">= 1"), "got: {err}");
}

#[test]
fn random_line_string_jitter_out_of_range_rejected() {
    let call = mk_call(
        "randomLineString",
        vec![],
        vec![
            ("bbox".into(), array(&[0.0, 0.0, 10.0, 10.0])),
            ("jitter".into(), Value::Number(1.5)),
        ],
    );
    let err = expect_err(bind(&call));
    assert!(err.to_string().contains("[0.0, 1.0]"), "got: {err}");
}

#[test]
fn random_polygon_is_closed_and_ccw() {
    let call = mk_call(
        "randomPolygon",
        vec![],
        vec![
            ("bbox".into(), array(&[0.0, 0.0, 10.0, 10.0])),
            ("vertices".into(), Value::Number(5.0)),
        ],
    );
    let g = produce_geom(call, 1);
    match g {
        Geometry::Polygon { rings } => {
            assert_eq!(rings.len(), 1, "single ring");
            let r = &rings[0];
            assert_eq!(r.len(), 6, "5 unique + 1 closing vertex");
            assert_eq!(r.first(), r.last(), "ring must be closed");
            // Shoelace must be positive (CCW).
            let mut area = 0.0;
            for i in 0..r.len() - 1 {
                area += r[i].0 * r[i + 1].1 - r[i + 1].0 * r[i].1;
            }
            assert!(area > 0.0, "polygon must be CCW, got signed area {area}");
        }
        other => panic!("expected Polygon, got {other:?}"),
    }
}

#[test]
fn random_polygon_rejects_too_few_vertices() {
    let call = mk_call(
        "randomPolygon",
        vec![],
        vec![
            ("bbox".into(), array(&[0.0, 0.0, 10.0, 10.0])),
            ("vertices".into(), Value::Number(2.0)),
        ],
    );
    let err = expect_err(bind(&call));
    assert!(err.to_string().contains(">= 3"), "got: {err}");
}

#[test]
fn random_bbox_is_rectangle_inside_within() {
    let within = [3.3, 50.7, 7.2, 53.5];
    let call = mk_call(
        "randomBbox",
        vec![],
        vec![
            ("within".into(), array(&within)),
            ("min_size_deg".into(), Value::Number(0.05)),
            ("max_size_deg".into(), Value::Number(0.5)),
        ],
    );
    let mut g = bind(&call).unwrap();
    let mut rng = SeedRng::from_seed(2);
    for _ in 0..50 {
        match g.produce(&mut rng, &ctx(0)) {
            Cell::Geometry(Geometry::Polygon { rings }) => {
                let r = &rings[0];
                assert_eq!(r.len(), 5, "rectangle = 4 unique + 1 closing");
                assert_eq!(r.first(), r.last(), "closed");
                for (lon, lat) in r {
                    assert!((within[0]..=within[2]).contains(lon), "lon out: {lon}");
                    assert!((within[1]..=within[3]).contains(lat), "lat out: {lat}");
                }
            }
            other => panic!("expected Polygon, got {other:?}"),
        }
    }
}

#[test]
fn random_bbox_rejects_min_larger_than_max() {
    let call = mk_call(
        "randomBbox",
        vec![],
        vec![
            ("within".into(), array(&[0.0, 0.0, 10.0, 10.0])),
            ("min_size_deg".into(), Value::Number(5.0)),
            ("max_size_deg".into(), Value::Number(1.0)),
        ],
    );
    let err = expect_err(bind(&call));
    assert!(err.to_string().contains("must be <="), "got: {err}");
}

#[test]
fn random_bbox_rejects_within_too_small() {
    let call = mk_call(
        "randomBbox",
        vec![],
        vec![
            ("within".into(), array(&[0.0, 0.0, 0.005, 0.005])),
            ("min_size_deg".into(), Value::Number(0.1)),
            ("max_size_deg".into(), Value::Number(0.2)),
        ],
    );
    let err = expect_err(bind(&call));
    assert!(err.to_string().contains("too small"), "got: {err}");
}

#[test]
fn random_choice_rejects_array_literal() {
    // Phase 2: arrays are now valid Value variants but randomChoice still
    // only accepts scalars. Catch this at bind time.
    let call = mk_call(
        "randomChoice",
        vec![Value::String("a".into()), array(&[1.0, 2.0])],
        vec![],
    );
    let err = expect_err(bind(&call));
    assert!(err.to_string().contains("array"), "got: {err}");
}

#[test]
fn unknown_kwarg_lists_allowed_names() {
    let call = mk_call(
        "randomBool",
        vec![],
        vec![("weigth".into(), Value::Number(0.5))], // typo
    );
    let err = expect_err(bind(&call));
    let msg = err.to_string();
    assert!(msg.contains("unknown keyword argument"), "got: {msg}");
    assert!(msg.contains("weight"), "must list allowed kwargs: {msg}");
}

#[test]
fn ref_bind_accepts_per_parent_kwarg() {
    use crate::ast::{Call, Value};
    let call = Call {
        function: "ref".into(),
        positional: vec![Value::ColumnRef { table: "users".into(), column: "id".into() }],
        kwargs: vec![("per_parent".into(), Value::Range { lo: 1, hi: 5 })],
        line: 1,
        col: 1,
    };
    assert!(crate::generators::bind(&call).is_ok());
}

#[test]
fn ref_bind_rejects_non_range_per_parent() {
    use crate::ast::{Call, Value};
    let call = Call {
        function: "ref".into(),
        positional: vec![Value::ColumnRef { table: "users".into(), column: "id".into() }],
        kwargs: vec![("per_parent".into(), Value::Number(5.0))],
        line: 1,
        col: 1,
    };
    let err = crate::generators::bind(&call).err().expect("should reject");
    assert!(matches!(err, crate::SemanticError::TypeMismatch { .. }));
}

#[test]
fn ref_bind_rejects_negative_per_parent() {
    use crate::ast::{Call, Value};
    let call = Call {
        function: "ref".into(),
        positional: vec![Value::ColumnRef { table: "users".into(), column: "id".into() }],
        kwargs: vec![("per_parent".into(), Value::Range { lo: -1, hi: 5 })],
        line: 1,
        col: 1,
    };
    let err = crate::generators::bind(&call).err().expect("should reject");
    assert!(matches!(err, crate::SemanticError::InvalidArgValue { .. }));
}