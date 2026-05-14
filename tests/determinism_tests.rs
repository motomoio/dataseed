//! Byte-for-byte determinism. The spec promises that the same `.dataseed`
//! file + same `--seed` produces identical output every run, on every
//! platform. We use `ChaCha8Rng` precisely because it's algorithm-stable
//! across `rand` versions; this test would catch any accidental drift.

use std::path::Path;
use std::process::Command;

fn dataseed_bin() -> &'static str {
    env!("CARGO_BIN_EXE_dataseed")
}

fn run_plant(file: &str, seed: u64, extra: &[&str]) -> Vec<u8> {
    let mut cmd = Command::new(dataseed_bin());
    cmd.arg("plant").arg(file).arg("--seed").arg(seed.to_string());
    for a in extra {
        cmd.arg(a);
    }
    let out = cmd.output().expect("run dataseed plant");
    assert!(
        out.status.success(),
        "plant failed: status={:?}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    out.stdout
}

#[test]
fn same_seed_same_output_sql() {
    let path = "examples/trees.dataseed";
    assert!(Path::new(path).exists(), "fixture missing: {path}");
    let a = run_plant(path, 42, &["--count", "100"]);
    let b = run_plant(path, 42, &["--count", "100"]);
    assert_eq!(a, b, "same seed must produce byte-identical output");
}

#[test]
fn different_seeds_produce_different_output() {
    let path = "examples/trees.dataseed";
    let a = run_plant(path, 1, &["--count", "100"]);
    let b = run_plant(path, 2, &["--count", "100"]);
    assert_ne!(a, b, "different seeds should diverge");
}

#[test]
fn cross_format_determinism() {
    // The same JSON example should also be byte-stable.
    let path = "examples/users.dataseed";
    if !Path::new(path).exists() {
        // Created during the same task — skip gracefully if generated after.
        return;
    }
    let a = run_plant(path, 99, &["--count", "50"]);
    let b = run_plant(path, 99, &["--count", "50"]);
    assert_eq!(a, b);
}

// ---------- Phase 2: geospatial determinism -------------------------------

#[test]
fn geospatial_examples_deterministic() {
    // Each new generator type must produce byte-identical output for the
    // same seed across runs. This guards against accidental nondeterminism
    // (e.g. HashMap iteration, system time, untracked RNG draws).
    for path in &[
        "examples/fields.dataseed",
        "examples/sensor_locations.dataseed",
        "examples/bike_routes.dataseed",
    ] {
        assert!(Path::new(path).exists(), "fixture missing: {path}");
        let a = run_plant(path, 42, &["--count", "20"]);
        let b = run_plant(path, 42, &["--count", "20"]);
        assert_eq!(a, b, "seed-stable output broken for {path}");
    }
}

#[test]
fn different_seeds_produce_different_geometries() {
    let path = "examples/fields.dataseed";
    let a = run_plant(path, 1, &["--count", "20"]);
    let b = run_plant(path, 2, &["--count", "20"]);
    assert_ne!(a, b, "different seeds must diverge for geometry too");
}

// ---------- Phase 3: multi-table + refs -----------------------------------

const SHOP_SHA: &str = "f13feb9a2a3275f4faaca6e8186a69ce169100023af5bf2ab34d4783f42b22f3";
const FLEET_SHA: &str = "2eacca59c9eee642ed3ce542e08372bd44b3534bf1b28bdfc2d3801d631255f5";

fn sha256(bytes: &[u8]) -> String {
    // Avoid pulling sha2 in as a runtime dep — shell out to the same tool
    // the README documents (`shasum`), which is available on macOS/Linux.
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new("shasum")
        .args(["-a", "256"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn shasum");
    child.stdin.as_mut().unwrap().write_all(bytes).unwrap();
    let out = child.wait_with_output().unwrap();
    let s = String::from_utf8(out.stdout).unwrap();
    s.split_whitespace().next().unwrap().to_string()
}

#[test]
fn shop_example_matches_reference_sha() {
    let out = run_plant("examples/shop.dataseed", 42, &[]);
    assert_eq!(sha256(&out), SHOP_SHA, "shop.dataseed reference SHA drifted");
}

#[test]
fn fleet_example_matches_reference_sha() {
    let out = run_plant("examples/fleet.dataseed", 42, &[]);
    assert_eq!(sha256(&out), FLEET_SHA, "fleet.dataseed reference SHA drifted");
}

#[test]
fn multi_table_is_seed_stable() {
    for path in ["examples/shop.dataseed", "examples/fleet.dataseed"] {
        let a = run_plant(path, 1, &[]);
        let b = run_plant(path, 1, &[]);
        assert_eq!(a, b, "seed-stable output broken for {path}");
    }
}

#[test]
fn ref_values_only_draw_from_materialized_parents() {
    // Run shop.dataseed and inspect the orders rows: every user_id must be
    // in the set of declared user IDs. This catches orphan-ref bugs (e.g.
    // wrong pool key, off-by-one indexing).
    let out = run_plant("examples/shop.dataseed", 42, &[]);
    let text = String::from_utf8(out).unwrap();

    let mut user_ids = std::collections::BTreeSet::new();
    for line in text.lines() {
        // users rows: `  (N, '...', '...', '...')...`
        if let Some(after_paren) = line.strip_prefix("  (") {
            if let Some(comma) = after_paren.find(',') {
                if let Ok(n) = after_paren[..comma].parse::<i64>() {
                    user_ids.insert(n);
                }
            }
        }
        if line.starts_with("-- Table: orders") {
            break;
        }
    }
    assert!(!user_ids.is_empty(), "should have parsed some user IDs");

    // Now read orders rows and check user_id (second column) is in user_ids.
    let mut in_orders = false;
    let mut checked = 0;
    for line in text.lines() {
        if line.starts_with("-- Table: orders") {
            in_orders = true;
            continue;
        }
        if !in_orders {
            continue;
        }
        if let Some(after_paren) = line.strip_prefix("  (") {
            let parts: Vec<&str> = after_paren.split(',').collect();
            if parts.len() >= 2 {
                let user_id: i64 = parts[1].trim().parse().expect("orders.user_id integer");
                assert!(
                    user_ids.contains(&user_id),
                    "orders.user_id={user_id} not in users.id set"
                );
                checked += 1;
            }
        }
    }
    assert!(checked > 0, "should have checked some orders rows");
}

#[test]
fn table_filter_does_not_change_filtered_rows() {
    // With --table orders, only the orders block is emitted, but the
    // values must match what the full run produces. (Implementation:
    // users are still generated and pooled so ref() draws are identical.)
    let full = run_plant("examples/shop.dataseed", 42, &[]);
    let filtered = run_plant("examples/shop.dataseed", 42, &["--table", "orders"]);
    let full_text = String::from_utf8(full).unwrap();
    let filt_text = String::from_utf8(filtered).unwrap();

    // Extract just the orders INSERT block from `full`.
    let orders_start = full_text.find("-- Table: orders").expect("orders header");
    let full_orders = &full_text[orders_start..];

    assert_eq!(
        full_orders.trim_end(),
        filt_text.trim_end(),
        "--table orders output must match the orders block of a full run"
    );
}

#[test]
fn wkt_and_geojson_coordinates_round_trip() {
    // The same generator + seed, emitted as WKT (via SQL) and as GeoJSON
    // (via JSON), must agree on the rounded-to-7dp coordinate values.
    use std::fs;
    use tempfile::TempDir;

    let tmp = TempDir::new().expect("temp dir");

    let sql_src = "output: sql\ntable: t\nschema { p: randomPoint(bbox: [3.3, 50.7, 7.2, 53.5]) }\ngenerate 5\n";
    let json_src = "output: json\ntable: t\nschema { p: randomPoint(bbox: [3.3, 50.7, 7.2, 53.5]) }\ngenerate 5\n";

    let sql_path = tmp.path().join("a.dataseed");
    let json_path = tmp.path().join("b.dataseed");
    fs::write(&sql_path, sql_src).unwrap();
    fs::write(&json_path, json_src).unwrap();

    let sql_out = String::from_utf8(
        run_plant(sql_path.to_str().unwrap(), 7, &[])
    ).unwrap();
    let json_out = String::from_utf8(
        run_plant(json_path.to_str().unwrap(), 7, &[])
    ).unwrap();

    // Extract WKT POINT coords (5 rows) from SQL output.
    let mut wkt_coords = Vec::new();
    for line in sql_out.lines() {
        if let Some(start) = line.find("POINT(") {
            let rest = &line[start + 6..];
            let end = rest.find(')').expect("WKT POINT must close");
            let body = &rest[..end];
            let mut parts = body.split_whitespace();
            let lon: f64 = parts.next().unwrap().parse().unwrap();
            let lat: f64 = parts.next().unwrap().parse().unwrap();
            wkt_coords.push((lon, lat));
        }
    }
    assert_eq!(wkt_coords.len(), 5);

    // Extract GeoJSON coords from JSON output.
    let parsed: serde_json::Value = serde_json::from_str(&json_out).expect("valid JSON");
    let arr = parsed.as_array().expect("top-level array");
    assert_eq!(arr.len(), 5);
    let mut json_coords = Vec::new();
    for row in arr {
        let coords = &row["p"]["coordinates"];
        let lon = coords[0].as_f64().unwrap();
        let lat = coords[1].as_f64().unwrap();
        json_coords.push((lon, lat));
    }

    assert_eq!(wkt_coords, json_coords, "WKT and GeoJSON must agree on rounded coords");
}
