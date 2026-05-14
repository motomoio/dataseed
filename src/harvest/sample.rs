//! Row sampling and per-column statistics.
//!
//! Strategy: for each table we run two passes.
//!
//! 1. A bounded `SELECT col1::text, col2::text, … LIMIT N` (PK-ordered when
//!    possible) that streams rows back as `Option<String>` for every
//!    column. Casting to text in SQL lets us avoid pulling chrono / uuid /
//!    decimal feature-flag families into the postgres crate. From these
//!    rows we compute null counts, distinct counts, observed decimals
//!    (for numeric formatting), and the true-fraction for booleans.
//!
//! 2. A single aggregate query per "needs stats" column kind: MIN/MAX
//!    server-side, ST_Extent and observed geometry types via PostGIS.
//!    Aggregates run against the full table so a 1k sample still produces
//!    accurate min/max bounds.

use std::collections::BTreeSet;

use postgres::Client;

use crate::harvest::connect::quote_ident;
use crate::harvest::model::{
    ColumnSample, HarvestSchema, HarvestTable, NumericStats, PgType, SamplingStrategy,
};

/// Cap on distinct values we store in [`ColumnSample::values`]. The
/// low-cardinality rule fires at ≤ 20 distinct values, so 64 leaves
/// plenty of margin while preventing unbounded growth on highly-unique
/// text columns.
const MAX_DISTINCT_VALUES: usize = 64;

pub fn populate(
    client: &mut Client,
    schema: &mut HarvestSchema,
    sample_size: usize,
) -> Result<(), String> {
    for table in &mut schema.tables {
        if table.sampling == SamplingStrategy::NoSample {
            continue;
        }
        sample_table(client, &schema.source.schema, table, sample_size)?;
        if schema.geometry_supported {
            populate_geometry_stats(client, &schema.source.schema, table)?;
        }
        populate_numeric_aggregates(client, &schema.source.schema, table)?;
    }
    Ok(())
}

fn sample_table(
    client: &mut Client,
    schema: &str,
    table: &mut HarvestTable,
    sample_size: usize,
) -> Result<(), String> {
    // Project every column as `col::text` so we get back `Option<String>`
    // for every cell. Geometry columns are pulled as NULL (we don't need
    // sample values for them — the geom_meta path handles type/bbox).
    let select_parts: Vec<String> = table
        .columns
        .iter()
        .map(|c| {
            let q = quote_ident(&c.name);
            if matches!(c.pg_type, PgType::Geometry) {
                // Cast to NULL::text so the column slot stays in the row
                // shape but doesn't pull the geometry payload over the wire.
                format!("NULL::text AS {q}")
            } else {
                format!("({q})::text AS {q}")
            }
        })
        .collect();

    let order_clause = match table.sampling {
        SamplingStrategy::PkOrdered => {
            let pk_cols = table
                .primary_key
                .iter()
                .map(|c| quote_ident(c))
                .collect::<Vec<_>>()
                .join(", ");
            format!("ORDER BY {pk_cols}")
        }
        SamplingStrategy::CtidOrdered => "ORDER BY ctid".to_string(),
        SamplingStrategy::NoSample => return Ok(()),
    };

    let sql = format!(
        "SELECT {} FROM {}.{} {order_clause} LIMIT {sample_size}",
        select_parts.join(", "),
        quote_ident(schema),
        quote_ident(&table.name),
    );

    let rows = client.query(&sql, &[]).map_err(|e| e.to_string())?;

    let n_cols = table.columns.len();
    let mut nulls = vec![0usize; n_cols];
    let mut bool_true = vec![0usize; n_cols];
    let mut bool_seen = vec![0usize; n_cols];
    let mut decimals = vec![0u8; n_cols];
    let mut distinct_full: Vec<BTreeSet<String>> =
        (0..n_cols).map(|_| BTreeSet::new()).collect();
    let mut distinct_overflow = vec![false; n_cols];
    let mut rows_seen = 0usize;

    for row in &rows {
        rows_seen += 1;
        for (i, col) in table.columns.iter().enumerate() {
            let raw: Option<String> = row.try_get(i).ok().flatten();
            match raw {
                None => nulls[i] += 1,
                Some(s) => {
                    let normalised = normalise_value(&s, &col.pg_type);
                    if matches!(col.pg_type, PgType::Boolean) {
                        bool_seen[i] += 1;
                        if normalised == "true" {
                            bool_true[i] += 1;
                        }
                    }
                    if matches!(col.pg_type, PgType::Real { .. }) {
                        if let Some(d) = decimals_in(&normalised) {
                            decimals[i] = decimals[i].max(d);
                        }
                    }
                    // Cap distinct-value growth at MAX_DISTINCT_VALUES so a
                    // highly-unique column doesn't balloon memory, but DO
                    // keep the values we already captured — pattern rules
                    // (email, UUID, name) need a representative sample to
                    // run their regexes against.
                    if distinct_full[i].len() < MAX_DISTINCT_VALUES {
                        distinct_full[i].insert(normalised);
                    } else if !distinct_full[i].contains(&normalised) {
                        distinct_overflow[i] = true;
                    }
                }
            }
        }
    }

    for (i, col) in table.columns.iter_mut().enumerate() {
        col.sample = ColumnSample {
            rows_examined: rows_seen,
            null_count: nulls[i],
            distinct_count: if distinct_overflow[i] {
                // Sentinel: more than the cap. Exact figure would need a
                // second pass — the only rule that cares about an exact
                // count is low-cardinality, which fires only at ≤ 20.
                MAX_DISTINCT_VALUES + 1
            } else {
                distinct_full[i].len()
            },
            values: distinct_full[i].iter().cloned().collect(),
            stats: NumericStats {
                min: None,
                max: None,
                observed_decimals: if matches!(col.pg_type, PgType::Real { .. })
                    && decimals[i] > 0
                {
                    Some(decimals[i])
                } else {
                    None
                },
                true_fraction: if matches!(col.pg_type, PgType::Boolean) && bool_seen[i] > 0
                {
                    Some(bool_true[i] as f64 / bool_seen[i] as f64)
                } else {
                    None
                },
            },
        };
    }

    Ok(())
}

/// Postgres' default text rendering for booleans is `t`/`f`; date/timestamp
/// types come back in their canonical formats but timestamps include the
/// time component which we don't want for ISO-date pattern detection.
fn normalise_value(raw: &str, ty: &PgType) -> String {
    match ty {
        PgType::Boolean => match raw {
            "t" => "true".to_string(),
            "f" => "false".to_string(),
            other => other.to_string(),
        },
        PgType::Date => raw.to_string(),
        PgType::Timestamp { .. } => {
            // Cut at first space — "2024-05-13 10:23:45+00" → "2024-05-13"
            raw.split_whitespace()
                .next()
                .unwrap_or(raw)
                .to_string()
        }
        _ => raw.to_string(),
    }
}

fn decimals_in(s: &str) -> Option<u8> {
    s.split_once('.').map(|(_, frac)| {
        // Strip a possible "e±N" exponent before counting digits.
        let frac = frac.split(|c: char| !c.is_ascii_digit()).next().unwrap_or("");
        frac.len().min(255) as u8
    })
}

fn populate_numeric_aggregates(
    client: &mut Client,
    schema: &str,
    table: &mut HarvestTable,
) -> Result<(), String> {
    let targets: Vec<usize> = table
        .columns
        .iter()
        .enumerate()
        .filter(|(_, c)| {
            matches!(
                c.pg_type,
                PgType::Integer { .. }
                    | PgType::Real { .. }
                    | PgType::Date
                    | PgType::Timestamp { .. }
            )
        })
        .map(|(i, _)| i)
        .collect();
    if targets.is_empty() {
        return Ok(());
    }

    let select_parts: Vec<String> = targets
        .iter()
        .flat_map(|&i| {
            let name = &table.columns[i].name;
            let q = quote_ident(name);
            let (minify, maxify): (String, String) = match table.columns[i].pg_type {
                PgType::Date | PgType::Timestamp { .. } => (
                    format!("to_char(MIN({q}), 'YYYY-MM-DD')"),
                    format!("to_char(MAX({q}), 'YYYY-MM-DD')"),
                ),
                _ => (format!("MIN({q})::text"), format!("MAX({q})::text")),
            };
            vec![minify, maxify]
        })
        .collect();

    let sql = format!(
        "SELECT {} FROM {}.{}",
        select_parts.join(", "),
        quote_ident(schema),
        quote_ident(&table.name)
    );

    let row = client.query_one(&sql, &[]).map_err(|e| e.to_string())?;
    for (slot, &col_idx) in targets.iter().enumerate() {
        let min_idx = slot * 2;
        let max_idx = slot * 2 + 1;
        let min_val: Option<String> = row.try_get(min_idx).ok().flatten();
        let max_val: Option<String> = row.try_get(max_idx).ok().flatten();
        let col = &mut table.columns[col_idx];
        col.sample.stats.min = min_val;
        col.sample.stats.max = max_val;

        if let PgType::Real { scale: Some(s), .. } = col.pg_type {
            col.sample.stats.observed_decimals = Some(s);
        }
    }

    Ok(())
}

fn populate_geometry_stats(
    client: &mut Client,
    schema: &str,
    table: &mut HarvestTable,
) -> Result<(), String> {
    let geom_indexes: Vec<usize> = table
        .columns
        .iter()
        .enumerate()
        .filter(|(_, c)| matches!(c.pg_type, PgType::Geometry))
        .map(|(i, _)| i)
        .collect();

    for idx in geom_indexes {
        let col_name = table.columns[idx].name.clone();
        let q = quote_ident(&col_name);
        let t = quote_ident(&table.name);
        let s = quote_ident(schema);

        let bbox_sql = format!(
            "SELECT
                ST_XMin(ext)::float8 AS xmin,
                ST_YMin(ext)::float8 AS ymin,
                ST_XMax(ext)::float8 AS xmax,
                ST_YMax(ext)::float8 AS ymax
             FROM (SELECT ST_Extent({q}) AS ext FROM {s}.{t}) sub"
        );
        let bbox_row = client.query_one(&bbox_sql, &[]).map_err(|e| e.to_string())?;
        let xmin: Option<f64> = bbox_row.try_get("xmin").ok().flatten();
        let ymin: Option<f64> = bbox_row.try_get("ymin").ok().flatten();
        let xmax: Option<f64> = bbox_row.try_get("xmax").ok().flatten();
        let ymax: Option<f64> = bbox_row.try_get("ymax").ok().flatten();
        let bbox = match (xmin, ymin, xmax, ymax) {
            (Some(a), Some(b), Some(c), Some(d)) => Some([a, b, c, d]),
            _ => None,
        };

        let types_sql = format!(
            "SELECT DISTINCT GeometryType({q}) AS gtype
             FROM {s}.{t}
             WHERE {q} IS NOT NULL
             LIMIT 16"
        );
        let type_rows = client.query(&types_sql, &[]).map_err(|e| e.to_string())?;
        let mut observed_types = BTreeSet::new();
        for r in type_rows {
            if let Ok(Some(s)) = r.try_get::<_, Option<String>>("gtype") {
                observed_types.insert(s);
            }
        }

        let avg_sql = format!(
            "SELECT
                ROUND(AVG(ST_NPoints({q})))::int AS avg_pts
             FROM (
                SELECT {q} FROM {s}.{t}
                WHERE {q} IS NOT NULL
                LIMIT 1000
             ) sub"
        );
        let avg_row = client.query_one(&avg_sql, &[]).map_err(|e| e.to_string())?;
        let avg_pts: Option<i32> = avg_row.try_get("avg_pts").ok().flatten();

        let geom = table.columns[idx].geom_meta.get_or_insert(
            crate::harvest::model::GeomMeta {
                declared_type: None,
                observed_types: Default::default(),
                srid: None,
                bbox: None,
                avg_segments: None,
                avg_vertices: None,
            },
        );
        geom.observed_types = observed_types.clone();
        geom.bbox = bbox;

        let single_type = if observed_types.len() == 1 {
            observed_types.iter().next().cloned()
        } else {
            None
        };
        match (single_type.as_deref(), avg_pts) {
            (Some("LINESTRING"), Some(n)) if n >= 2 => {
                geom.avg_segments = Some((n as u32 - 1).max(2));
            }
            (Some("POLYGON"), Some(n)) if n >= 4 => {
                geom.avg_vertices = Some((n as u32 - 1).max(3));
            }
            _ => {}
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimals_counts_fractional_digits() {
        assert_eq!(decimals_in("3.14"), Some(2));
        assert_eq!(decimals_in("3"), None);
        assert_eq!(decimals_in("3.140"), Some(3));
    }

    #[test]
    fn normalise_boolean_text() {
        assert_eq!(normalise_value("t", &PgType::Boolean), "true");
        assert_eq!(normalise_value("f", &PgType::Boolean), "false");
    }

    #[test]
    fn normalise_timestamp_strips_time() {
        assert_eq!(
            normalise_value("2024-05-13 10:23:45+00", &PgType::Timestamp { with_tz: true }),
            "2024-05-13"
        );
    }
}
