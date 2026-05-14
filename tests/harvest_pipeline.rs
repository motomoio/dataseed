//! End-to-end harvest pipeline test without a real database.
//!
//! Builds a `HarvestSchema` by hand (as if introspection had just run),
//! feeds it through `infer` and `emit`, then parses + semantically checks
//! the rendered output using dataseed's own parser. A failure here means
//! the harvest pipeline produced something `plant` won't accept — which
//! would be a Phase 4 regression.

use std::collections::BTreeSet;

use chrono::TimeZone;
use dataseed::ast::OutputKind;
use dataseed::harvest::emit;
use dataseed::harvest::infer::{self, default_output_mode};
use dataseed::harvest::model::{
    ColumnDefault, ColumnSample, ForeignKey, GeomMeta, HarvestColumn, HarvestSchema,
    HarvestTable, NumericStats, PgType, SamplingStrategy, SourceInfo,
};

fn col(
    name: &str,
    ty: PgType,
    sample: ColumnSample,
    nullable: bool,
) -> HarvestColumn {
    HarvestColumn {
        name: name.to_string(),
        pg_type: ty,
        is_nullable: nullable,
        default: ColumnDefault::None,
        identity: false,
        sample,
        geom_meta: None,
    }
}

fn full_sample(rows: usize, values: Vec<&str>, min: Option<&str>, max: Option<&str>) -> ColumnSample {
    let mut vs: Vec<String> = values.iter().map(|s| s.to_string()).collect();
    vs.sort();
    let n = vs.len();
    ColumnSample {
        rows_examined: rows,
        null_count: 0,
        distinct_count: n,
        values: vs,
        stats: NumericStats {
            min: min.map(String::from),
            max: max.map(String::from),
            ..Default::default()
        },
    }
}

#[test]
fn end_to_end_pipeline_against_a_realistic_schema() {
    // users: id (PK/identity), name (name pattern), email (email pattern),
    //        signup_date (date), active (boolean).
    let mut users_id = HarvestColumn::bare("id", PgType::Integer { width: 4 });
    users_id.identity = true;
    users_id.sample = full_sample(100, vec![], Some("1"), Some("100"));

    let users_name = col(
        "name",
        PgType::Text,
        full_sample(
            100,
            (0..40)
                .map(|i| ["Alice Smith", "Bob Jones", "Carol Khan", "Dan Reilly"][i % 4])
                .collect(),
            None,
            None,
        ),
        false,
    );
    let users_email = col(
        "email",
        PgType::Text,
        full_sample(
            100,
            (0..30)
                .map(|i| Box::leak(format!("user{i}@example.com").into_boxed_str()) as &str)
                .collect(),
            None,
            None,
        ),
        false,
    );
    let users_signup = col(
        "signup_date",
        PgType::Date,
        full_sample(100, vec![], Some("2021-03-12"), Some("2026-05-13")),
        false,
    );
    let mut users_active = col(
        "active",
        PgType::Boolean,
        ColumnSample {
            rows_examined: 100,
            null_count: 3,
            distinct_count: 2,
            values: vec!["false".into(), "true".into()],
            stats: NumericStats {
                true_fraction: Some(0.87),
                ..Default::default()
            },
        },
        true,
    );
    users_active.is_nullable = true;

    let users = HarvestTable {
        name: "users".into(),
        columns: vec![users_id, users_name, users_email, users_signup, users_active],
        primary_key: vec!["id".into()],
        foreign_keys: vec![],
        estimated_rows: 1247,
        sampling: SamplingStrategy::PkOrdered,
    };

    // orders: id (PK), user_id (FK → users.id), total (numeric),
    //         status (low-cardinality), delivery_location (PostGIS POINT),
    //         created_at (timestamp).
    let mut orders_id = HarvestColumn::bare("id", PgType::Integer { width: 4 });
    orders_id.identity = true;
    let orders_user_id = col(
        "user_id",
        PgType::Integer { width: 4 },
        full_sample(100, vec![], Some("1"), Some("1000")),
        false,
    );
    let orders_total = col(
        "total",
        PgType::Real {
            is_double: true,
            scale: Some(2),
        },
        full_sample(100, vec![], Some("0.5"), Some("4287.99")),
        false,
    );
    let orders_status = col(
        "status",
        PgType::Text,
        full_sample(
            100,
            (0..80)
                .map(|i| ["cancelled", "delivered", "pending", "shipped"][i % 4])
                .collect(),
            None,
            None,
        ),
        false,
    );
    let mut orders_loc = HarvestColumn::bare("delivery_location", PgType::Geometry);
    orders_loc.sample.rows_examined = 100;
    orders_loc.geom_meta = Some(GeomMeta {
        declared_type: Some("POINT".into()),
        observed_types: {
            let mut s = BTreeSet::new();
            s.insert("POINT".into());
            s
        },
        srid: Some(4326),
        bbox: Some([3.3140, 50.7510, 7.2275, 53.5550]),
        avg_segments: None,
        avg_vertices: None,
    });
    let orders_created = col(
        "created_at",
        PgType::Timestamp { with_tz: true },
        full_sample(100, vec![], Some("2022-01-01"), Some("2026-05-13")),
        false,
    );

    let orders = HarvestTable {
        name: "orders".into(),
        columns: vec![
            orders_id,
            orders_user_id,
            orders_total,
            orders_status,
            orders_loc,
            orders_created,
        ],
        primary_key: vec!["id".into()],
        foreign_keys: vec![ForeignKey {
            constraint_name: "orders_user_id_fkey".into(),
            columns: vec!["user_id".into()],
            ref_table: "users".into(),
            ref_columns: vec!["id".into()],
        }],
        estimated_rows: 8932,
        sampling: SamplingStrategy::PkOrdered,
    };

    let schema = HarvestSchema {
        source: SourceInfo {
            database: "shop".into(),
            schema: "public".into(),
            harvested_at: chrono::Utc.with_ymd_and_hms(2026, 5, 14, 12, 0, 0).unwrap(),
            invocation: "dataseed harvest postgres://shop".into(),
        },
        tables: vec![orders, users.clone()], // intentionally mis-ordered
        geometry_supported: true,
    };

    let inference = infer::infer_schema(&schema);
    let mode = default_output_mode(&schema);
    assert_eq!(mode, OutputKind::Postgis);

    let rendered = emit::render(&schema, &inference, mode, 1.0).expect("render");
    emit::self_check(&rendered).expect("rendered text must parse + pass semantic");

    // Sanity: users must be declared before orders in the output.
    let users_pos = rendered.find("table users").expect("users block");
    let orders_pos = rendered.find("table orders").expect("orders block");
    assert!(
        users_pos < orders_pos,
        "users must be emitted before orders (topo order)"
    );

    // The FK should produce a ref() call.
    assert!(rendered.contains("user_id: ref(users.id)"));
    // The geometry column should produce randomPoint with a bbox.
    assert!(rendered.contains("delivery_location: randomPoint("));
    // The timestamp downgrade comment should appear.
    assert!(rendered.contains("TODO: source is timestamp"));
    // The null annotation for `active` should appear.
    assert!(rendered.contains("source allows NULL"));
}

#[test]
fn determinism_two_runs_produce_identical_output() {
    // Same input, two render calls → identical bytes (after stripping the
    // "harvested" timestamp, which is taken from the source struct as-is).
    let mut id_col = HarvestColumn::bare("id", PgType::Integer { width: 4 });
    id_col.identity = true;
    let table = HarvestTable {
        name: "t".into(),
        columns: vec![
            id_col,
            col(
                "label",
                PgType::Text,
                full_sample(80, vec!["a", "b", "c"], None, None),
                false,
            ),
        ],
        primary_key: vec!["id".into()],
        foreign_keys: vec![],
        estimated_rows: 50,
        sampling: SamplingStrategy::PkOrdered,
    };
    let schema = HarvestSchema {
        source: SourceInfo {
            database: "x".into(),
            schema: "public".into(),
            harvested_at: chrono::Utc.with_ymd_and_hms(2026, 5, 14, 12, 0, 0).unwrap(),
            invocation: "fixture".into(),
        },
        tables: vec![table],
        geometry_supported: false,
    };
    let inf1 = infer::infer_schema(&schema);
    let inf2 = infer::infer_schema(&schema);
    let r1 = emit::render(&schema, &inf1, OutputKind::Sql, 1.0).unwrap();
    let r2 = emit::render(&schema, &inf2, OutputKind::Sql, 1.0).unwrap();
    assert_eq!(r1, r2);
}

#[test]
fn cycle_emits_warning_comment() {
    let a_to_b = HarvestTable {
        name: "a".into(),
        columns: vec![{
            let mut c = HarvestColumn::bare("id", PgType::Integer { width: 4 });
            c.identity = true;
            c
        }],
        primary_key: vec!["id".into()],
        foreign_keys: vec![ForeignKey {
            constraint_name: "fk".into(),
            columns: vec!["id".into()],
            ref_table: "b".into(),
            ref_columns: vec!["id".into()],
        }],
        estimated_rows: 1,
        sampling: SamplingStrategy::PkOrdered,
    };
    let b_to_a = HarvestTable {
        name: "b".into(),
        columns: vec![{
            let mut c = HarvestColumn::bare("id", PgType::Integer { width: 4 });
            c.identity = true;
            c
        }],
        primary_key: vec!["id".into()],
        foreign_keys: vec![ForeignKey {
            constraint_name: "fk".into(),
            columns: vec!["id".into()],
            ref_table: "a".into(),
            ref_columns: vec!["id".into()],
        }],
        estimated_rows: 1,
        sampling: SamplingStrategy::PkOrdered,
    };
    let schema = HarvestSchema {
        source: SourceInfo {
            database: "cycle".into(),
            schema: "public".into(),
            harvested_at: chrono::Utc.with_ymd_and_hms(2026, 5, 14, 12, 0, 0).unwrap(),
            invocation: "fixture".into(),
        },
        tables: vec![a_to_b, b_to_a],
        geometry_supported: false,
    };
    let inf = infer::infer_schema(&schema);
    let rendered = emit::render(&schema, &inf, OutputKind::Sql, 1.0).unwrap();
    assert!(rendered.contains("WARNING: source schema has a FK cycle"));
}
