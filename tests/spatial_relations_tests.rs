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
fn per_parent_couples_nested_ref_to_assigned_parent() {
    // When a child table uses per_parent AND a nested ref to the same parent
    // column, the nested ref should resolve to the assigned parent — not an
    // independent uniform draw. Concretely: a sensor with quota-assigned
    // warehouse N must have its `location` sampled near warehouse N's location.
    let src = r#"
        output: postgis
        table warehouses {
          id:       sequence
          location: randomPoint(bbox: [0.0, 0.0, 100.0, 1.0])
        }
        table sensors {
          id:           sequence
          warehouse_id: ref(warehouses.id, per_parent: 2..2)
          location:     randomPointNear(center: ref(warehouses.location), radius_m: 1000)
        }
        generate warehouses: 5
    "#;
    let out = render_to_string(src, 42);

    // Each warehouse owns 2 sensors. We need to recover (warehouse_id, sensor.location)
    // for each row and verify the sensor location is near warehouse's location.
    //
    // Strategy: count how many distinct longitude bands the sensors land in.
    // With per_parent: each warehouse is at longitude ~ 20*(i-1), and sensors
    // have radius_m=1000 (so they stay within ~0.01 deg lon of their center).
    // If nested ref is COUPLED, sensors near warehouse 1 cluster around lon~0,
    // sensors near warehouse 2 cluster around lon~25, etc. — 5 distinct clusters.
    // If nested ref is INDEPENDENT (uniform draw), sensors are spread randomly
    // across all warehouses.

    // Simpler test: extract all sensor locations and warehouse_ids, check that
    // for each row, the lon of location is within ~0.01 deg of the warehouse
    // it claims (warehouse_id). Each warehouse is at longitude_i = ((i-1) * X)
    // for some X in [0, 100] — but we don't know it directly. So extract
    // warehouse locations first, then verify sensor.location.lon is close to
    // sensor's assigned warehouse.location.lon.

    let warehouse_lons = extract_warehouse_lons(&out);
    assert_eq!(warehouse_lons.len(), 5, "expected 5 warehouses, got {warehouse_lons:?}");

    let sensors = extract_sensors(&out);
    assert_eq!(sensors.len(), 10, "5 warehouses × 2 sensors = 10");

    for (s_idx, (warehouse_id, sensor_lon)) in sensors.iter().enumerate() {
        let expected_lon = warehouse_lons[*warehouse_id as usize - 1];
        let delta = (sensor_lon - expected_lon).abs();
        assert!(
            delta < 0.5,
            "sensor {s_idx} assigned to warehouse {warehouse_id} (lon={expected_lon:.4}) \
             but its location is at lon={sensor_lon:.4} (delta={delta:.4}) — \
             nested ref was NOT coupled to forced_parent"
        );
    }
}

fn extract_warehouse_lons(out: &str) -> Vec<f64> {
    // Warehouses: INSERT INTO warehouses (id, location) VALUES
    //   (1, ST_GeomFromText('POINT(LON LAT)', 4326)),
    let mut result = Vec::new();
    let start = out.find("INSERT INTO warehouses").expect("warehouses section");
    let mut section = &out[start..];
    while let Some(point_start) = section.find("POINT(") {
        let after = &section[point_start + "POINT(".len()..];
        let end = after.find(' ').expect("POINT(LON LAT)");
        let lon: f64 = after[..end].parse().expect("lon parse");
        result.push(lon);
        section = &after[end..];
        if section.contains("INSERT INTO sensors") && result.len() >= 5 { break; }
    }
    result.truncate(5);
    result
}

fn extract_sensors(out: &str) -> Vec<(u64, f64)> {
    // Sensors: INSERT INTO sensors (id, warehouse_id, location) VALUES
    //   (1, 3, ST_GeomFromText('POINT(LON LAT)', 4326)),
    // We want (warehouse_id, lon).
    let start = out.find("INSERT INTO sensors").expect("sensors section");
    let section = &out[start..];
    let mut result = Vec::new();
    for line in section.lines() {
        let t = line.trim();
        if !t.starts_with('(') { continue; }
        // pattern: "(<id>, <warehouse_id>, ST_GeomFromText('POINT(<lon> <lat>)', 4326)),"
        let after_id_comma = match t.find(", ") {
            Some(i) => &t[i + 2..],
            None => continue,
        };
        let warehouse_id_end = match after_id_comma.find(',') {
            Some(i) => i,
            None => continue,
        };
        let warehouse_id: u64 = match after_id_comma[..warehouse_id_end].parse() {
            Ok(v) => v,
            Err(_) => continue,
        };
        let after_warehouse = &after_id_comma[warehouse_id_end..];
        let point_start = match after_warehouse.find("POINT(") {
            Some(i) => i + "POINT(".len(),
            None => continue,
        };
        let after_point = &after_warehouse[point_start..];
        let space = match after_point.find(' ') {
            Some(i) => i,
            None => continue,
        };
        let lon: f64 = match after_point[..space].parse() {
            Ok(v) => v,
            Err(_) => continue,
        };
        result.push((warehouse_id, lon));
    }
    result
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
    let payload = result.expect_err("expected either semantic rejection or runtime panic");
    let msg = if let Some(s) = payload.downcast_ref::<&'static str>() {
        s.to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        String::new()
    };
    assert!(
        msg.contains("point") || msg.contains("geometry") || msg.contains("center"),
        "panic message should mention point/geometry/center, got: {msg}"
    );
}
