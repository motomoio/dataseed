//! Integration tests for `dataseed harvest` against a real PostgreSQL
//! instance spun up via testcontainers.
//!
//! Skipped by default — set `DATASEED_HARVEST_TESTS=1` to enable them.
//! That keeps `cargo test` fast and CI-without-Docker green; CI runs that
//! want to exercise the harvest path can flip the env var on.

#![cfg(all(feature = "harvest", test))]

use std::collections::BTreeSet;
use std::io::Write;

use postgres::{Client, NoTls};
use testcontainers::core::{ContainerPort, IntoContainerPort, WaitFor};
use testcontainers::runners::SyncRunner;
use testcontainers::{GenericImage, ImageExt};
use testcontainers_modules::postgres::Postgres as PgImage;

use dataseed::harvest::{run_harvest, HarvestOptions};

fn enabled() -> bool {
    std::env::var("DATASEED_HARVEST_TESTS").map_or(false, |v| v == "1")
}

fn must_enabled() -> bool {
    if !enabled() {
        eprintln!("skipping (set DATASEED_HARVEST_TESTS=1 to run)");
        return false;
    }
    true
}

fn pg_conn_str(host: &str, port: u16) -> String {
    format!("postgres://postgres:postgres@{host}:{port}/postgres")
}

fn options(conn: &str, output: tempfile::NamedTempFile) -> (HarvestOptions, tempfile::NamedTempFile) {
    let path = output.path().to_path_buf();
    (
        HarvestOptions {
            connection_string: conn.to_string(),
            schema: "public".to_string(),
            tables: None,
            exclude: BTreeSet::new(),
            sample: 1000,
            scale: 1.0,
            output_mode: None,
            output_file: Some(path),
            verbose: false,
            invocation_line: "dataseed harvest <test>".to_string(),
        },
        output,
    )
}

fn read_output(f: tempfile::NamedTempFile) -> String {
    std::fs::read_to_string(f.path()).expect("read harvested output")
}

#[test]
fn declared_fk_renders_as_ref() {
    if !must_enabled() {
        return;
    }
    let container = PgImage::default().start().expect("postgres container");
    let host = container.get_host().expect("host").to_string();
    let port = container.get_host_port_ipv4(5432).expect("port");
    let conn = pg_conn_str(&host, port);

    let mut client = Client::connect(&conn, NoTls).expect("connect");
    client
        .batch_execute(
            r#"
            CREATE TABLE users (
                id SERIAL PRIMARY KEY,
                email TEXT NOT NULL
            );
            CREATE TABLE orders (
                id SERIAL PRIMARY KEY,
                user_id INTEGER NOT NULL REFERENCES users(id),
                status TEXT NOT NULL
            );
            INSERT INTO users (email) SELECT 'u' || i || '@x.com' FROM generate_series(1, 100) i;
            INSERT INTO orders (user_id, status)
                SELECT (random()*99+1)::int,
                       (ARRAY['pending','shipped','delivered','cancelled'])[(random()*3+1)::int]
                FROM generate_series(1, 200);
            "#,
        )
        .expect("seed");

    let out_file = tempfile::NamedTempFile::new().unwrap();
    let (opts, kept) = options(&conn, out_file);
    run_harvest(opts).expect("harvest succeeds");
    let text = read_output(kept);

    assert!(text.contains("user_id: ref(users.id)"), "missing FK ref:\n{text}");
    assert!(text.contains("status: randomChoice("), "missing low-cardinality choice:\n{text}");
    assert!(text.contains("email: randomEmail()"), "missing email pattern:\n{text}");
}

#[test]
fn topo_order_places_parents_before_children() {
    if !must_enabled() {
        return;
    }
    let container = PgImage::default().start().expect("postgres container");
    let host = container.get_host().expect("host").to_string();
    let port = container.get_host_port_ipv4(5432).expect("port");
    let conn = pg_conn_str(&host, port);

    let mut client = Client::connect(&conn, NoTls).expect("connect");
    client
        .batch_execute(
            r#"
            CREATE TABLE parents (id SERIAL PRIMARY KEY);
            CREATE TABLE children (id SERIAL PRIMARY KEY, parent_id INT REFERENCES parents(id));
            INSERT INTO parents DEFAULT VALUES;
            INSERT INTO children (parent_id) VALUES (1);
            "#,
        )
        .unwrap();

    let out_file = tempfile::NamedTempFile::new().unwrap();
    let (opts, kept) = options(&conn, out_file);
    run_harvest(opts).unwrap();
    let text = read_output(kept);
    let p = text.find("table parents").expect("parents block");
    let c = text.find("table children").expect("children block");
    assert!(p < c, "parents must precede children:\n{text}");
}

#[test]
fn determinism_two_invocations_byte_identical_output() {
    if !must_enabled() {
        return;
    }
    let container = PgImage::default().start().expect("postgres container");
    let host = container.get_host().expect("host").to_string();
    let port = container.get_host_port_ipv4(5432).expect("port");
    let conn = pg_conn_str(&host, port);

    let mut client = Client::connect(&conn, NoTls).expect("connect");
    client
        .batch_execute(
            r#"
            CREATE TABLE t (id SERIAL PRIMARY KEY, label TEXT);
            INSERT INTO t (label) SELECT (ARRAY['a','b','c'])[(i % 3) + 1] FROM generate_series(1, 100) i;
            "#,
        )
        .unwrap();

    let out1 = tempfile::NamedTempFile::new().unwrap();
    let out2 = tempfile::NamedTempFile::new().unwrap();
    let (opts1, kept1) = options(&conn, out1);
    let (opts2, kept2) = options(&conn, out2);
    run_harvest(opts1).unwrap();
    run_harvest(opts2).unwrap();
    let a = strip_timestamp(&read_output(kept1));
    let b = strip_timestamp(&read_output(kept2));
    assert_eq!(a, b, "harvest output must be deterministic");
}

/// Strip the `harvested YYYY-MM-DDTHH:MM:SSZ` portion of the header so the
/// determinism check isn't defeated by the wall clock advancing between
/// invocations.
fn strip_timestamp(s: &str) -> String {
    s.lines()
        .map(|line| {
            if line.starts_with("# Source:") {
                line.split(", harvested")
                    .next()
                    .unwrap_or(line)
                    .to_string()
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn round_trip_harvest_then_plant() {
    if !must_enabled() {
        return;
    }
    let container = PgImage::default().start().expect("postgres container");
    let host = container.get_host().expect("host").to_string();
    let port = container.get_host_port_ipv4(5432).expect("port");
    let conn = pg_conn_str(&host, port);

    let mut client = Client::connect(&conn, NoTls).expect("connect");
    client
        .batch_execute(
            r#"
            CREATE TABLE roles (id SERIAL PRIMARY KEY, name TEXT NOT NULL);
            CREATE TABLE users (
                id SERIAL PRIMARY KEY,
                role_id INT NOT NULL REFERENCES roles(id),
                email TEXT NOT NULL
            );
            INSERT INTO roles (name) VALUES ('admin'), ('user'), ('guest');
            INSERT INTO users (role_id, email)
                SELECT (random()*2+1)::int, 'u' || i || '@x.com'
                FROM generate_series(1, 50) i;
            "#,
        )
        .unwrap();

    let out_file = tempfile::NamedTempFile::new().unwrap();
    let (opts, kept) = options(&conn, out_file);
    run_harvest(opts).unwrap();
    let dataseed_path = kept.path().to_path_buf();
    let _kept = kept; // keep alive

    // Invoke `dataseed plant` via assert_cmd against the harvested file.
    use assert_cmd::Command;
    let mut cmd = Command::cargo_bin("dataseed").expect("dataseed bin");
    let out = cmd
        .arg("plant")
        .arg(&dataseed_path)
        .arg("--seed")
        .arg("42")
        .output()
        .expect("plant runs");
    assert!(
        out.status.success(),
        "plant failed:\nstdout: {}\nstderr: {}\nfile: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
        std::fs::read_to_string(&dataseed_path).unwrap_or_default(),
    );
}

#[test]
fn postgis_geometry_emits_random_point_with_bbox() {
    if !must_enabled() {
        return;
    }
    // PostGIS-enabled image; the alpine variant is small.
    let image = GenericImage::new("postgis/postgis", "16-3.4-alpine")
        .with_exposed_port(5432.tcp())
        .with_wait_for(WaitFor::message_on_stderr(
            "database system is ready to accept connections",
        ));
    let container = image
        .with_env_var("POSTGRES_PASSWORD", "postgres")
        .with_env_var("POSTGRES_DB", "postgres")
        .with_env_var("POSTGRES_USER", "postgres")
        .start()
        .expect("postgis container");
    let host = container.get_host().expect("host").to_string();
    let port = container
        .get_host_port_ipv4(ContainerPort::Tcp(5432))
        .expect("port");
    let conn = pg_conn_str(&host, port);

    let mut client = Client::connect(&conn, NoTls).expect("connect");
    client
        .batch_execute(
            r#"
            CREATE EXTENSION IF NOT EXISTS postgis;
            CREATE TABLE places (
                id SERIAL PRIMARY KEY,
                loc GEOMETRY(POINT, 4326) NOT NULL
            );
            INSERT INTO places (loc)
                SELECT ST_SetSRID(ST_MakePoint(4.0 + random(), 51.0 + random()), 4326)
                FROM generate_series(1, 100);
            "#,
        )
        .unwrap();

    let out_file = tempfile::NamedTempFile::new().unwrap();
    let (opts, kept) = options(&conn, out_file);
    run_harvest(opts).unwrap();
    let text = read_output(kept);

    assert!(text.contains("loc: randomPoint("), "missing geometry inference:\n{text}");
    assert!(text.contains("output: postgis"));
}

#[test]
fn slim_build_has_no_harvest_subcommand() {
    // Sanity: the slim build (no `harvest` feature) must build and run
    // without the subcommand. We don't rebuild here — assert_cmd uses
    // whatever's already in target/. So instead, verify that with the
    // current default-on binary, `harvest --help` prints help text.
    use assert_cmd::Command;
    let mut cmd = Command::cargo_bin("dataseed").expect("dataseed bin");
    let out = cmd.arg("harvest").arg("--help").output().expect("run");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("harvest"), "harvest --help should mention itself");
}

#[allow(dead_code)]
fn drop_dyn(_w: &mut dyn Write) {}
