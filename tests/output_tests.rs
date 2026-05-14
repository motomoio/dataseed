//! Output emitter tests: SQL and JSON shapes from small fixtures.

use dataseed::output::render;
use dataseed::parser::parse;
use dataseed::rng::SeedRng;

fn render_str(src: &str, seed: u64) -> String {
    let file = parse(src).expect("parse ok");
    let mut buf = Vec::new();
    let mut rng = SeedRng::from_seed(seed);
    let count = file.generate[0].count;
    render(&file, count, &mut rng, &mut buf).expect("render ok");
    String::from_utf8(buf).expect("utf-8 output")
}

// ---------- SQL -----------------------------------------------------------

#[test]
fn sql_basic_shape() {
    let src = r#"
        output: sql
        table: trees
        schema {
          id: sequence
          species: randomChoice("oak", "birch")
        }
        generate 3
    "#;
    let out = render_str(src, 1);
    assert!(out.starts_with("INSERT INTO trees (id, species) VALUES\n"), "got:\n{out}");
    assert!(out.contains("(1, '"), "first row should have id 1: {out}");
    assert!(out.contains("(3, '"), "third row should have id 3: {out}");
    assert!(out.trim_end().ends_with(';'), "INSERT must end with `;`: {out}");
}

#[test]
fn sql_escapes_single_quotes() {
    // randomChoice gives us a deterministic single-quote-bearing string.
    let src = r#"
        output: sql
        table: t
        schema { x: randomChoice("o'brien") }
        generate 1
    "#;
    let out = render_str(src, 0);
    assert!(out.contains("'o''brien'"), "single quote must be doubled: {out}");
}

#[test]
fn sql_zero_rows_emits_comment_not_empty_insert() {
    let src = "output: sql\ntable: t\nschema { x: sequence }\ngenerate 0\n";
    let out = render_str(src, 0);
    assert!(out.contains("0 rows"), "zero-row output should be a comment: {out}");
    assert!(!out.contains("INSERT"), "no INSERT for zero rows: {out}");
}

#[test]
fn sql_boolean_renders_uppercase() {
    let src = "output: sql\ntable: t\nschema { ok: randomBool(weight: 1.0) }\ngenerate 1\n";
    let out = render_str(src, 0);
    assert!(out.contains("(TRUE)"), "true must render as TRUE: {out}");
}

#[test]
fn sql_batches_at_1000() {
    // 1500 rows → exactly two INSERT statements (1000 + 500).
    let src = "output: sql\ntable: t\nschema { id: sequence }\ngenerate 1500\n";
    let out = render_str(src, 0);
    let inserts = out.matches("INSERT INTO").count();
    assert_eq!(inserts, 2, "1500 rows should split into 2 INSERT batches, got:\n{out}");
    let semicolons = out.matches(';').count();
    assert_eq!(semicolons, 2, "each batch ends in `;`");
}

// ---------- JSON ----------------------------------------------------------

#[test]
fn json_is_valid() {
    let src = r#"
        output: json
        table: t
        schema {
          id: sequence
          species: randomChoice("oak", "birch")
          height: randomRealNumber(1.0, 10.0, decimals: 2)
        }
        generate 4
    "#;
    let out = render_str(src, 7);
    let parsed: serde_json::Value =
        serde_json::from_str(&out).unwrap_or_else(|e| panic!("invalid JSON: {e}\n{out}"));
    let arr = parsed.as_array().expect("top-level array");
    assert_eq!(arr.len(), 4);
    let first = &arr[0];
    assert_eq!(first["id"], 1, "sequence starts at 1");
    assert!(first["species"].is_string());
    assert!(first["height"].is_number());
}

#[test]
fn json_zero_rows_is_empty_array() {
    let src = "output: json\ntable: t\nschema { x: sequence }\ngenerate 0\n";
    let out = render_str(src, 0);
    let parsed: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
    assert_eq!(parsed.as_array().unwrap().len(), 0);
}

#[test]
fn json_boolean_renders_as_boolean_not_string() {
    let src = "output: json\ntable: t\nschema { ok: randomBool(weight: 1.0) }\ngenerate 1\n";
    let out = render_str(src, 0);
    let parsed: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
    assert_eq!(parsed[0]["ok"], true);
}
