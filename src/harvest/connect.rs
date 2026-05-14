//! Postgres connection and schema introspection.
//!
//! All public functions return `Result<_, String>` so the orchestrator
//! doesn't need a custom error enum just for this phase — errors are
//! displayed verbatim to the user.

use std::collections::{BTreeMap, BTreeSet};

use chrono::Utc;
use postgres::{Client, NoTls};

use crate::harvest::model::{
    ColumnDefault, ForeignKey, GeomMeta, HarvestColumn, HarvestSchema, HarvestTable,
    PgType, SamplingStrategy, SourceInfo,
};

pub fn open(conn_str: &str) -> Result<Client, String> {
    // TLS support requires the `harvest-tls` feature; without it we error
    // when the user passes sslmode=require. Most local-dev runs are
    // trust/md5/scram over a unix socket, which doesn't need TLS.
    Client::connect(conn_str, NoTls).map_err(|e| e.to_string())
}

pub fn detect_postgis(client: &mut Client) -> Result<bool, String> {
    let row = client
        .query_one(
            "SELECT EXISTS(SELECT 1 FROM pg_extension WHERE extname = 'postgis')",
            &[],
        )
        .map_err(|e| e.to_string())?;
    let exists: bool = row.get(0);
    Ok(exists)
}

pub fn introspect(
    client: &mut Client,
    schema: &str,
    only: Option<&[String]>,
    exclude: &BTreeSet<String>,
    geometry_supported: bool,
) -> Result<HarvestSchema, String> {
    let database = current_database(client)?;

    let table_names = list_tables(client, schema)?
        .into_iter()
        .filter(|t| only.map_or(true, |sel| sel.iter().any(|s| s == t)))
        .filter(|t| !exclude.contains(t))
        .collect::<Vec<_>>();

    let mut tables = Vec::with_capacity(table_names.len());
    for name in &table_names {
        let columns = list_columns(client, schema, name, geometry_supported)?;
        let primary_key = list_primary_key(client, schema, name)?;
        let foreign_keys = list_foreign_keys(client, schema, name)?;
        let estimated_rows = count_rows(client, schema, name)?;
        let sampling = if estimated_rows == 0 {
            SamplingStrategy::NoSample
        } else if !primary_key.is_empty() {
            SamplingStrategy::PkOrdered
        } else {
            SamplingStrategy::CtidOrdered
        };
        tables.push(HarvestTable {
            name: name.clone(),
            columns,
            primary_key,
            foreign_keys,
            estimated_rows,
            sampling,
        });
    }

    Ok(HarvestSchema {
        source: SourceInfo {
            database,
            schema: schema.to_string(),
            harvested_at: Utc::now(),
            // Overwritten by the orchestrator with the redacted CLI line.
            invocation: String::new(),
        },
        tables,
        geometry_supported,
    })
}

fn current_database(client: &mut Client) -> Result<String, String> {
    let row = client
        .query_one("SELECT current_database()", &[])
        .map_err(|e| e.to_string())?;
    Ok(row.get(0))
}

fn list_tables(client: &mut Client, schema: &str) -> Result<Vec<String>, String> {
    // Base tables only — skip views, materialized views, foreign tables.
    let rows = client
        .query(
            "SELECT table_name FROM information_schema.tables
             WHERE table_schema = $1 AND table_type = 'BASE TABLE'
             ORDER BY table_name",
            &[&schema],
        )
        .map_err(|e| e.to_string())?;
    Ok(rows.iter().map(|r| r.get::<_, String>(0)).collect())
}

fn list_columns(
    client: &mut Client,
    schema: &str,
    table: &str,
    geometry_supported: bool,
) -> Result<Vec<HarvestColumn>, String> {
    let rows = client
        .query(
            "SELECT
                column_name,
                data_type,
                udt_name,
                is_nullable,
                column_default,
                is_identity,
                numeric_precision,
                numeric_scale
             FROM information_schema.columns
             WHERE table_schema = $1 AND table_name = $2
             ORDER BY ordinal_position",
            &[&schema, &table],
        )
        .map_err(|e| e.to_string())?;

    let geom_meta_map = if geometry_supported {
        load_geometry_columns(client, schema, table)?
    } else {
        BTreeMap::new()
    };

    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let name: String = r.get("column_name");
        let data_type: String = r.get("data_type");
        let udt_name: String = r.get("udt_name");
        let nullable_str: String = r.get("is_nullable");
        let default_str: Option<String> = r.get("column_default");
        let identity_str: String = r.get("is_identity");
        let numeric_scale: Option<i32> = r.get("numeric_scale");

        let pg_type = classify_pg_type(&data_type, &udt_name, numeric_scale);
        let is_nullable = nullable_str == "YES";
        let identity = identity_str == "YES";
        let default = classify_default(default_str.as_deref());

        let geom_meta = if matches!(pg_type, PgType::Geometry) {
            geom_meta_map.get(&name).cloned()
        } else {
            None
        };

        out.push(HarvestColumn {
            name,
            pg_type,
            is_nullable,
            default,
            identity,
            sample: Default::default(),
            geom_meta,
        });
    }

    Ok(out)
}

fn classify_pg_type(data_type: &str, udt_name: &str, scale: Option<i32>) -> PgType {
    // `data_type` is the SQL-standard name, `udt_name` the Postgres-internal
    // one. For geometry/uuid we need to consult `udt_name` because
    // `data_type` reports 'USER-DEFINED'.
    match data_type {
        "smallint" => PgType::Integer { width: 2 },
        "integer" => PgType::Integer { width: 4 },
        "bigint" => PgType::Integer { width: 8 },
        "real" => PgType::Real {
            is_double: false,
            scale: None,
        },
        "double precision" => PgType::Real {
            is_double: true,
            scale: None,
        },
        "numeric" => PgType::Real {
            is_double: false,
            scale: scale.and_then(|s| if s >= 0 { Some(s as u8) } else { None }),
        },
        "text" | "character varying" | "character" => PgType::Text,
        "boolean" => PgType::Boolean,
        "date" => PgType::Date,
        "timestamp without time zone" => PgType::Timestamp { with_tz: false },
        "timestamp with time zone" => PgType::Timestamp { with_tz: true },
        "uuid" => PgType::Uuid,
        "USER-DEFINED" if udt_name == "geometry" || udt_name == "geography" => {
            PgType::Geometry
        }
        _ => PgType::Other(if data_type == "USER-DEFINED" {
            udt_name.to_string()
        } else {
            data_type.to_string()
        }),
    }
}

fn classify_default(raw: Option<&str>) -> ColumnDefault {
    let Some(raw) = raw else { return ColumnDefault::None };
    let trimmed = raw.trim();
    if trimmed.starts_with("nextval(") {
        ColumnDefault::Sequence
    } else {
        ColumnDefault::Expr(trimmed.to_string())
    }
}

fn list_primary_key(
    client: &mut Client,
    schema: &str,
    table: &str,
) -> Result<Vec<String>, String> {
    // Pull PK columns in their declared order via pg_index/pg_attribute.
    // information_schema works too but the path through pg_catalog is
    // simpler and faster.
    let rows = client
        .query(
            "SELECT a.attname
             FROM pg_index i
             JOIN pg_class c ON c.oid = i.indrelid
             JOIN pg_namespace n ON n.oid = c.relnamespace
             JOIN pg_attribute a ON a.attrelid = c.oid
                                AND a.attnum = ANY(i.indkey)
             WHERE i.indisprimary
               AND n.nspname = $1
               AND c.relname = $2
             ORDER BY array_position(i.indkey, a.attnum)",
            &[&schema, &table],
        )
        .map_err(|e| e.to_string())?;
    Ok(rows.iter().map(|r| r.get::<_, String>(0)).collect())
}

fn list_foreign_keys(
    client: &mut Client,
    schema: &str,
    table: &str,
) -> Result<Vec<ForeignKey>, String> {
    // Group columns by constraint name so composite FKs come back as one
    // row each. We order columns within a constraint by their position.
    let rows = client
        .query(
            "SELECT
                tc.constraint_name,
                kcu.column_name,
                ccu.table_name  AS ref_table,
                ccu.column_name AS ref_column,
                kcu.ordinal_position
             FROM information_schema.table_constraints tc
             JOIN information_schema.key_column_usage kcu
               ON tc.constraint_name = kcu.constraint_name
              AND tc.table_schema = kcu.table_schema
             JOIN information_schema.referential_constraints rc
               ON tc.constraint_name = rc.constraint_name
              AND tc.constraint_schema = rc.constraint_schema
             JOIN information_schema.constraint_column_usage ccu
               ON rc.unique_constraint_name = ccu.constraint_name
              AND rc.unique_constraint_schema = ccu.constraint_schema
             WHERE tc.constraint_type = 'FOREIGN KEY'
               AND tc.table_schema = $1
               AND tc.table_name = $2
             ORDER BY tc.constraint_name, kcu.ordinal_position",
            &[&schema, &table],
        )
        .map_err(|e| e.to_string())?;

    let mut grouped: BTreeMap<String, ForeignKey> = BTreeMap::new();
    for r in rows {
        let name: String = r.get("constraint_name");
        let col: String = r.get("column_name");
        let ref_table: String = r.get("ref_table");
        let ref_column: String = r.get("ref_column");
        let entry = grouped.entry(name.clone()).or_insert_with(|| ForeignKey {
            constraint_name: name,
            columns: Vec::new(),
            ref_table,
            ref_columns: Vec::new(),
        });
        entry.columns.push(col);
        entry.ref_columns.push(ref_column);
    }
    Ok(grouped.into_values().collect())
}

fn count_rows(client: &mut Client, schema: &str, table: &str) -> Result<u64, String> {
    let sql = format!(
        "SELECT COUNT(*) FROM {}.{}",
        quote_ident(schema),
        quote_ident(table)
    );
    let row = client.query_one(&sql, &[]).map_err(|e| e.to_string())?;
    let n: i64 = row.get(0);
    Ok(n.max(0) as u64)
}

fn load_geometry_columns(
    client: &mut Client,
    schema: &str,
    table: &str,
) -> Result<BTreeMap<String, GeomMeta>, String> {
    // PostGIS exposes per-column type/srid via the geometry_columns view.
    // It only covers `geometry` (not `geography`), so we still need to fall
    // back to sample-time ST_GeometryType detection for geography columns.
    let rows = client
        .query(
            "SELECT f_geometry_column, type, srid
             FROM geometry_columns
             WHERE f_table_schema = $1 AND f_table_name = $2",
            &[&schema, &table],
        )
        .map_err(|e| e.to_string())?;

    let mut out = BTreeMap::new();
    for r in rows {
        let col: String = r.get("f_geometry_column");
        let declared: String = r.get("type");
        let srid: i32 = r.get("srid");
        out.insert(
            col,
            GeomMeta {
                declared_type: if declared.is_empty() || declared == "GEOMETRY" {
                    None
                } else {
                    Some(declared)
                },
                observed_types: BTreeSet::new(),
                srid: if srid == 0 { None } else { Some(srid) },
                bbox: None,
                avg_segments: None,
                avg_vertices: None,
            },
        );
    }
    Ok(out)
}

/// Quote a Postgres identifier conservatively. Used for all dynamic table /
/// column names interpolated into SQL strings — `client.query` cannot bind
/// identifiers as parameters, only values.
pub fn quote_ident(s: &str) -> String {
    let escaped = s.replace('"', "\"\"");
    format!("\"{escaped}\"")
}
