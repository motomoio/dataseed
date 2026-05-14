//! Phase 4.3 — spatial relations.
//!
//! Tests for the killer feature of Task 3.2: `randomPointNear` accepts
//! `ref(table.column)` as `center`, resolving per-row from the parent's
//! geometry:point column. Also covers the semantic-check ref-edge walker
//! finding refs nested inside another generator's kwargs.

use dataseed::output::{render_plan, RenderPlan};
use dataseed::parser::parse;
use dataseed::rng::SeedRng;
use dataseed::semantic;
use std::collections::BTreeMap;

fn render_to_string(src: &str, seed: u64) -> String {
    let file = parse(src).expect("parse");
    let report = semantic::check(&file);
    assert!(report.is_ok(), "semantic errors: {:?}", report.errors);
    let counts: BTreeMap<String, u64> =
        file.generate.iter().map(|g| (g.table.clone(), g.count)).collect();
    let plan = RenderPlan {
        topo_order: report.topo_order.clone(),
        referenced: report.referenced.clone(),
        counts,
        emit_only: None,
        per_parent_owners: report.per_parent_owners.clone(),
    };
    let mut rng = SeedRng::from_seed(seed);
    let mut buf: Vec<u8> = Vec::new();
    render_plan(&file, &plan, &mut rng, &mut buf).expect("render");
    String::from_utf8(buf).unwrap()
}

#[test]
fn random_point_near_ref_resolves_per_row() {
    let src = r#"
        output: postgis
        table warehouses {
          id:       sequence
          location: randomPoint(bbox: [4.0, 51.0, 6.0, 52.0])
        }
        table sensors {
          id:       sequence
          parent:   ref(warehouses.id)
          location: randomPointNear(center: ref(warehouses.location), radius_m: 100)
        }
        generate warehouses: 3
        generate sensors: 30
    "#;
    let out = render_to_string(src, 42);
    // 30 sensors rows × one POINT in `location` column = 30 occurrences of "POINT(".
    // The 3 warehouses also have POINT in their location column.
    let near_count = out.matches("POINT(").count();
    assert!(near_count >= 33, "expected at least 33 POINTs (3 warehouses + 30 sensors), got {near_count}");
}

#[test]
fn random_point_near_with_literal_center_still_works() {
    // Regression: the literal-center path must keep producing the same output.
    let src = r#"
        output: sql
        table points {
          id:  sequence
          loc: randomPointNear(center: [5.0, 52.0], radius_m: 1000)
        }
        generate points: 5
    "#;
    let out = render_to_string(src, 42);
    assert!(out.contains("INSERT INTO points"));
    let near_count = out.matches("POINT(").count();
    assert_eq!(near_count, 5);
}

#[test]
fn semantic_check_collects_ref_edges_from_kwargs() {
    let src = r#"
        output: postgis
        table warehouses {
          id:       sequence
          location: randomPoint(bbox: [4.0, 51.0, 6.0, 52.0])
        }
        table sensors {
          id:       sequence
          location: randomPointNear(center: ref(warehouses.location), radius_m: 100)
        }
        generate warehouses: 1
        generate sensors: 1
    "#;
    let file = parse(src).unwrap();
    let report = semantic::check(&file);
    assert!(report.is_ok(), "{:?}", report.errors);
    // Topo order: warehouses comes before sensors because sensors depends on it.
    let w_idx = report.topo_order.iter().position(|t| t == "warehouses").unwrap();
    let s_idx = report.topo_order.iter().position(|t| t == "sensors").unwrap();
    assert!(w_idx < s_idx, "topo: {:?}", report.topo_order);
    // Materialization plan: warehouses.location must be retained for sensors to read.
    assert!(report.referenced.get("warehouses")
        .is_some_and(|s| s.contains("location")));
}

#[test]
fn random_point_near_rejects_ref_to_non_point_column() {
    // Negative test: ref to an id column (integer) should fail bind.
    let src = r#"
        output: sql
        table users { id: sequence }
        table sensors {
          id:       sequence
          location: randomPointNear(center: ref(users.id), radius_m: 100)
        }
        generate users: 1
        generate sensors: 1
    "#;
    let file = parse(src).unwrap();
    let report = semantic::check(&file);
    // Semantic check itself may not type-check the cast — bind is where this
    // is enforced. Either:
    //  * semantic check rejects (preferred — caught early), OR
    //  * bind succeeds but render panics with a clear message at run time.
    // Verify ONE of these happens. Cleanest is rejecting at semantic time
    // since the ref target's catalog return type is known.
    // We accept either result: errors present (preferred), or a panic at runtime.
    if !report.is_ok() {
        // Preferred outcome — semantic-time rejection.
        return;
    }
    // If semantic accepts, render should panic. Wrap in catch_unwind.
    let result = std::panic::catch_unwind(|| {
        render_to_string(src, 42);
    });
    assert!(result.is_err(), "expected either semantic rejection or runtime panic");
}
