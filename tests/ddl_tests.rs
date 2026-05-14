use assert_cmd::Command;
use predicates::str::contains;

#[test]
fn emit_ddl_prepends_create_table_for_shop_example() {
    Command::cargo_bin("dataseed").unwrap()
        .args(["plant", "examples/shop.dataseed", "--seed", "42", "--emit-ddl"])
        .assert()
        .success()
        .stdout(contains("CREATE TABLE users"))
        .stdout(contains("CREATE TABLE orders"))
        .stdout(contains("id BIGINT"))
        .stdout(contains("INSERT INTO users"));
}

#[test]
fn emit_ddl_for_postgis_emits_typed_geometry_columns() {
    Command::cargo_bin("dataseed").unwrap()
        .args(["plant", "examples/warehouses.dataseed", "--seed", "42", "--emit-ddl"])
        .assert()
        .success()
        .stdout(contains("CREATE TABLE warehouses"))
        .stdout(contains("location geometry(Point, 4326)"));
}

#[test]
fn emit_ddl_warns_for_json_output() {
    use std::io::Write;
    use tempfile::NamedTempFile;
    let mut f = NamedTempFile::new().unwrap();
    writeln!(f, "output: json").unwrap();
    writeln!(f, "table t {{ id: sequence }}").unwrap();
    writeln!(f, "generate t: 1").unwrap();
    Command::cargo_bin("dataseed").unwrap()
        .args(["plant", f.path().to_str().unwrap(), "--seed", "1", "--emit-ddl"])
        .assert()
        .success()
        .stderr(contains("--emit-ddl is a no-op for JSON"));
}

#[test]
fn no_emit_ddl_is_default_behavior() {
    // Without --emit-ddl, no CREATE TABLE in output.
    Command::cargo_bin("dataseed").unwrap()
        .args(["plant", "examples/shop.dataseed", "--seed", "42"])
        .assert()
        .success()
        .stdout(predicates::function::function(|s: &str| !s.contains("CREATE TABLE")));
}
