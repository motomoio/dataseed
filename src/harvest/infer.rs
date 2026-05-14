//! Pure inference: map a [`HarvestSchema`] to a set of generator calls.
//!
//! Every decision goes through [`infer_column`], whose 13-step rule order
//! mirrors the design in the PR description. The function returns both
//! the chosen generator (as an `ast::Call`) and an [`InferenceNote`]
//! describing why — the latter feeds the `--verbose` reasoning output.
//!
//! No database access here. The introspect+sample phases produce all the
//! data this module needs, so the entire rule table is testable with
//! hand-built `HarvestColumn` fixtures.

use std::collections::{BTreeMap, BTreeSet};

use crate::ast::{Call, OutputKind, Value};
use crate::harvest::model::{
    ColumnDefault, GeomMeta, HarvestColumn, HarvestSchema, HarvestTable, PgType,
};

/// Same threshold the spec calls out: ≤ 20 distinct values across at
/// least 50 rows → low-cardinality.
const LOW_CARDINALITY_DISTINCT: usize = 20;
const LOW_CARDINALITY_MIN_ROWS: usize = 50;
/// Minimum pattern match fraction for email/UUID/name/etc.
const PATTERN_MATCH_THRESHOLD: f64 = 0.9;

/// Result of running inference over a whole schema.
#[derive(Debug, Clone)]
pub struct InferenceOutput {
    pub tables: BTreeMap<String, InferredTable>,
    /// Per-table notes in the same order as `HarvestSchema.tables`.
    pub notes: Vec<TableNotes>,
}

#[derive(Debug, Clone)]
pub struct InferredTable {
    pub name: String,
    pub columns: Vec<InferredColumn>,
}

#[derive(Debug, Clone)]
pub struct InferredColumn {
    pub name: String,
    pub call: Call,
    /// Inline comment text shown after the generator on the same line
    /// (without the leading `#`). `None` means "no comment".
    pub inline_comment: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TableNotes {
    pub table: String,
    pub columns: Vec<InferenceNote>,
}

#[derive(Debug, Clone)]
pub struct InferenceNote {
    pub column: String,
    pub chosen_generator: String,
    pub reason: String,
}

impl InferenceOutput {
    /// Format `--verbose` lines: one column per line, tab-aligned.
    pub fn verbose_lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        // Compute fixed widths for stable alignment.
        let mut qual_width = 0usize;
        let mut gen_width = 0usize;
        for tn in &self.notes {
            for n in &tn.columns {
                qual_width =
                    qual_width.max(tn.table.len() + 1 + n.column.len());
                gen_width = gen_width.max(n.chosen_generator.len());
            }
        }
        for tn in &self.notes {
            for n in &tn.columns {
                let qualified = format!("{}.{}", tn.table, n.column);
                out.push(format!(
                    "{:<qual_width$}  →  {:<gen_width$}  ({})",
                    qualified,
                    n.chosen_generator,
                    n.reason,
                    qual_width = qual_width,
                    gen_width = gen_width,
                ));
            }
        }
        out
    }
}

pub fn infer_schema(schema: &HarvestSchema) -> InferenceOutput {
    let table_index: BTreeMap<&str, &HarvestTable> = schema
        .tables
        .iter()
        .map(|t| (t.name.as_str(), t))
        .collect();

    let mut tables = BTreeMap::new();
    let mut notes_vec = Vec::with_capacity(schema.tables.len());

    for table in &schema.tables {
        let mut inferred_cols = Vec::with_capacity(table.columns.len());
        let mut col_notes = Vec::with_capacity(table.columns.len());
        for col in &table.columns {
            let result = infer_column(table, col, &table_index);
            col_notes.push(InferenceNote {
                column: col.name.clone(),
                chosen_generator: function_label(&result.call),
                reason: result.reason,
            });
            inferred_cols.push(InferredColumn {
                name: col.name.clone(),
                call: result.call,
                inline_comment: result.inline_comment,
            });
        }
        tables.insert(
            table.name.clone(),
            InferredTable {
                name: table.name.clone(),
                columns: inferred_cols,
            },
        );
        notes_vec.push(TableNotes {
            table: table.name.clone(),
            columns: col_notes,
        });
    }

    InferenceOutput {
        tables,
        notes: notes_vec,
    }
}

/// Pick a default output mode if the user didn't specify one. PostGIS when
/// the source has any geometry columns, otherwise SQL.
pub fn default_output_mode(schema: &HarvestSchema) -> OutputKind {
    let any_geom = schema
        .tables
        .iter()
        .flat_map(|t| &t.columns)
        .any(|c| matches!(c.pg_type, PgType::Geometry));
    if any_geom {
        OutputKind::Postgis
    } else {
        OutputKind::Sql
    }
}

struct ColumnInference {
    call: Call,
    inline_comment: Option<String>,
    reason: String,
}

fn infer_column(
    table: &HarvestTable,
    col: &HarvestColumn,
    schema: &BTreeMap<&str, &HarvestTable>,
) -> ColumnInference {
    // The order here matches the design's 0..12 numbered rules.
    if let Some(c) = rule_declared_fk(table, col) {
        return finalize(col, c);
    }
    if let Some(c) = rule_sequence(table, col) {
        return finalize(col, c);
    }
    if let Some(c) = rule_heuristic_fk(table, col, schema) {
        return finalize(col, c);
    }
    if let Some(c) = rule_geometry(col) {
        return finalize(col, c);
    }
    if let Some(c) = rule_boolean(col) {
        return finalize(col, c);
    }
    if let Some(c) = rule_uuid(col) {
        return finalize(col, c);
    }
    if let Some(c) = rule_date(col) {
        return finalize(col, c);
    }
    if let Some(c) = rule_timestamp(col) {
        return finalize(col, c);
    }
    if let Some(c) = rule_integer(col) {
        return finalize(col, c);
    }
    if let Some(c) = rule_real(col) {
        return finalize(col, c);
    }
    if let Some(c) = rule_low_cardinality(col) {
        return finalize(col, c);
    }
    if let Some(c) = rule_text_pattern(col) {
        return finalize(col, c);
    }
    finalize(col, rule_fallback(col))
}

/// Append the nullability annotation (if any) to whatever inline comment
/// the rule produced. Keeps every rule's body focused on its own reasoning.
fn finalize(col: &HarvestColumn, mut c: ColumnInference) -> ColumnInference {
    if col.is_nullable && col.sample.rows_examined > 0 {
        let pct = (col.sample.null_count as f64 / col.sample.rows_examined as f64) * 100.0;
        let null_note = format!(
            "source allows NULL ({:.0}% null in sample); dataseed has no null generator yet",
            pct
        );
        c.inline_comment = Some(match c.inline_comment {
            Some(existing) if !existing.is_empty() => {
                format!("{existing}; {null_note}")
            }
            _ => null_note,
        });
    }
    c
}

fn rule_declared_fk(table: &HarvestTable, col: &HarvestColumn) -> Option<ColumnInference> {
    let fk = table.single_col_fk_for(&col.name)?;
    let ref_col = fk.ref_columns.first()?.clone();
    let call = ref_call(&fk.ref_table, &ref_col);
    Some(ColumnInference {
        call,
        inline_comment: Some("inferred from FK constraint".to_string()),
        reason: format!(
            "declared FK to {}.{}",
            fk.ref_table, ref_col
        ),
    })
}

fn rule_sequence(table: &HarvestTable, col: &HarvestColumn) -> Option<ColumnInference> {
    if !matches!(col.pg_type, PgType::Integer { .. }) {
        return None;
    }
    let is_pk = table.is_single_col_pk(&col.name);
    let is_seq_default = col.default == ColumnDefault::Sequence;
    let dense = looks_like_dense_sequence(col);

    if col.identity || is_seq_default || is_pk || dense {
        let reason = if col.identity {
            "GENERATED AS IDENTITY".to_string()
        } else if is_seq_default {
            "nextval() default".to_string()
        } else if is_pk {
            "integer single-column PK".to_string()
        } else {
            "values form a dense 1..N sequence".to_string()
        };
        return Some(ColumnInference {
            call: Call {
                function: "sequence".to_string(),
                positional: Vec::new(),
                kwargs: Vec::new(),
                line: 0,
                col: 0,
            },
            inline_comment: None,
            reason,
        });
    }
    None
}

fn looks_like_dense_sequence(col: &HarvestColumn) -> bool {
    // Cheap check: distinct count == rows_examined (all unique) AND every
    // sampled value parses as a positive integer with min == 1.
    if col.sample.rows_examined < 10 {
        return false;
    }
    if col.sample.distinct_count != col.sample.rows_examined {
        return false;
    }
    let Some(min) = col.sample.stats.min.as_deref() else {
        return false;
    };
    min == "1"
}

fn rule_heuristic_fk(
    table: &HarvestTable,
    col: &HarvestColumn,
    schema: &BTreeMap<&str, &HarvestTable>,
) -> Option<ColumnInference> {
    if !matches!(col.pg_type, PgType::Integer { .. }) {
        return None;
    }
    if !col.name.ends_with("_id") {
        return None;
    }
    let stem = &col.name[..col.name.len() - 3];
    // Skip if the column is in this same table — heuristic only crosses tables.
    let candidates = [stem.to_string(), format!("{stem}s")];
    let target = candidates
        .iter()
        .find(|c| c.as_str() != table.name && schema.contains_key(c.as_str()))?;
    let target_table = schema.get(target.as_str())?;

    // The target needs a single-column `id` of an integer type.
    let target_id = target_table.columns.iter().find(|c| c.name == "id")?;
    if !matches!(target_id.pg_type, PgType::Integer { .. }) {
        return None;
    }

    Some(ColumnInference {
        call: ref_call(target, "id"),
        inline_comment: Some("inferred from column name; no FK declared".to_string()),
        reason: format!("name `{}` matches table `{target}`", col.name),
    })
}

fn rule_geometry(col: &HarvestColumn) -> Option<ColumnInference> {
    if !matches!(col.pg_type, PgType::Geometry) {
        return None;
    }
    let meta = col.geom_meta.as_ref();
    let observed_or_declared_type = pick_geom_type(meta);

    let geom_type = match observed_or_declared_type {
        Some(t) => t,
        None => {
            return Some(ColumnInference {
                call: fallback_call(),
                inline_comment: Some(
                    "TODO: geometry column with no observable values; manual config needed"
                        .to_string(),
                ),
                reason: "geometry column has no non-null rows".to_string(),
            });
        }
    };

    let Some(bbox) = meta.and_then(|m| m.bbox) else {
        return Some(ColumnInference {
            call: fallback_call(),
            inline_comment: Some(
                "TODO: could not compute bounding box; manual config needed".to_string(),
            ),
            reason: "no bbox available".to_string(),
        });
    };
    let bbox_arr = round_bbox(bbox);

    let (function, extra_kwargs, reason) = match geom_type.as_str() {
        "POINT" => ("randomPoint", Vec::new(), "geometry POINT".to_string()),
        "LINESTRING" => {
            let segs = meta.and_then(|m| m.avg_segments).unwrap_or(5);
            (
                "randomLineString",
                vec![("segments".to_string(), Value::Number(segs as f64))],
                format!("geometry LINESTRING (avg {segs} segments)"),
            )
        }
        "POLYGON" => {
            let verts = meta.and_then(|m| m.avg_vertices).unwrap_or(6);
            (
                "randomPolygon",
                vec![("vertices".to_string(), Value::Number(verts as f64))],
                format!("geometry POLYGON (avg {verts} vertices)"),
            )
        }
        other => {
            return Some(ColumnInference {
                call: fallback_call(),
                inline_comment: Some(format!(
                    "TODO: geometry type `{other}` not auto-detected; manual config needed"
                )),
                reason: format!("unsupported geometry type `{other}`"),
            });
        }
    };

    // Mixed-type warning is appended to the reason regardless of which
    // geometry rule fired so verbose output stays informative.
    let mixed_warning = if let Some(m) = meta {
        if m.observed_types.len() > 1 {
            Some(format!(
                "TODO: mixed geometry types found ({}); manual config needed",
                m.observed_types.iter().cloned().collect::<Vec<_>>().join(" and ")
            ))
        } else {
            None
        }
    } else {
        None
    };

    let mut kwargs = vec![(
        "bbox".to_string(),
        Value::Array(bbox_arr.into_iter().map(Value::Number).collect()),
    )];
    kwargs.extend(extra_kwargs);

    let inline = mixed_warning.or_else(|| {
        Some(format!(
            "bbox inferred from {} rows",
            col.sample.rows_examined
        ))
    });

    Some(ColumnInference {
        call: Call {
            function: function.to_string(),
            positional: Vec::new(),
            kwargs,
            line: 0,
            col: 0,
        },
        inline_comment: inline,
        reason,
    })
}

fn pick_geom_type(meta: Option<&GeomMeta>) -> Option<String> {
    let m = meta?;
    if m.observed_types.len() == 1 {
        return m.observed_types.iter().next().cloned();
    }
    if !m.observed_types.is_empty() {
        // Mixed; the rule will surface a TODO. Pick alphabetically-first
        // so the rule body has something to work with.
        return m.observed_types.iter().next().cloned();
    }
    m.declared_type.clone()
}

fn round_bbox(bbox: [f64; 4]) -> [f64; 4] {
    let r = |v: f64| (v * 10_000.0).round() / 10_000.0;
    [r(bbox[0]), r(bbox[1]), r(bbox[2]), r(bbox[3])]
}

fn rule_boolean(col: &HarvestColumn) -> Option<ColumnInference> {
    if !matches!(col.pg_type, PgType::Boolean) {
        return None;
    }
    let frac = col.sample.stats.true_fraction.unwrap_or(0.5);
    let weight = (frac * 100.0).round() / 100.0;
    Some(ColumnInference {
        call: Call {
            function: "randomBool".to_string(),
            positional: Vec::new(),
            kwargs: vec![("weight".to_string(), Value::Number(weight))],
            line: 0,
            col: 0,
        },
        inline_comment: None,
        reason: format!("boolean ({:.0}% true in sample)", frac * 100.0),
    })
}

fn rule_uuid(col: &HarvestColumn) -> Option<ColumnInference> {
    let native = matches!(col.pg_type, PgType::Uuid);
    let pattern_match = matches!(col.pg_type, PgType::Text)
        && pattern_fraction(col, is_uuid) >= PATTERN_MATCH_THRESHOLD;
    if !native && !pattern_match {
        return None;
    }
    Some(ColumnInference {
        call: Call {
            function: "randomUuid".to_string(),
            positional: Vec::new(),
            kwargs: Vec::new(),
            line: 0,
            col: 0,
        },
        inline_comment: None,
        reason: if native {
            "native uuid type".to_string()
        } else {
            "≥90% of sample matches UUID pattern".to_string()
        },
    })
}

fn rule_date(col: &HarvestColumn) -> Option<ColumnInference> {
    if !matches!(col.pg_type, PgType::Date) {
        return None;
    }
    let (start, end) = pick_date_range(col, "1970-01-01", "2030-12-31");
    Some(ColumnInference {
        call: random_date_call(&start, &end),
        inline_comment: None,
        reason: format!("date type ({start} … {end})"),
    })
}

fn rule_timestamp(col: &HarvestColumn) -> Option<ColumnInference> {
    if !matches!(col.pg_type, PgType::Timestamp { .. }) {
        return None;
    }
    let (start, end) = pick_date_range(col, "1970-01-01", "2030-12-31");
    Some(ColumnInference {
        call: random_date_call(&start, &end),
        inline_comment: Some(
            "TODO: source is timestamp; dataseed currently emits dates only".to_string(),
        ),
        reason: format!("timestamp downgraded to date ({start} … {end})"),
    })
}

fn pick_date_range(col: &HarvestColumn, lo_default: &str, hi_default: &str) -> (String, String) {
    let s = col
        .sample
        .stats
        .min
        .clone()
        .unwrap_or_else(|| lo_default.to_string());
    let e = col
        .sample
        .stats
        .max
        .clone()
        .unwrap_or_else(|| hi_default.to_string());
    (s, e)
}

fn random_date_call(start: &str, end: &str) -> Call {
    Call {
        function: "randomDate".to_string(),
        positional: vec![
            Value::String(start.to_string()),
            Value::String(end.to_string()),
        ],
        kwargs: Vec::new(),
        line: 0,
        col: 0,
    }
}

fn rule_integer(col: &HarvestColumn) -> Option<ColumnInference> {
    if !matches!(col.pg_type, PgType::Integer { .. }) {
        return None;
    }
    let min = col
        .sample
        .stats
        .min
        .as_deref()
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);
    let max = col
        .sample
        .stats
        .max
        .as_deref()
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(if min == 0 { 100 } else { min + 100 });
    let max = max.max(min);
    Some(ColumnInference {
        call: Call {
            function: "randomInt".to_string(),
            positional: vec![Value::Number(min as f64), Value::Number(max as f64)],
            kwargs: Vec::new(),
            line: 0,
            col: 0,
        },
        inline_comment: None,
        reason: format!("integer ({min} … {max})"),
    })
}

fn rule_real(col: &HarvestColumn) -> Option<ColumnInference> {
    if !matches!(col.pg_type, PgType::Real { .. }) {
        return None;
    }
    let min = col
        .sample
        .stats
        .min
        .as_deref()
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0);
    let max = col
        .sample
        .stats
        .max
        .as_deref()
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(if min == 0.0 { 1.0 } else { min + 1.0 });
    let max = max.max(min);
    let decimals = col.sample.stats.observed_decimals.unwrap_or(2).max(1) as i64;
    Some(ColumnInference {
        call: Call {
            function: "randomRealNumber".to_string(),
            positional: vec![Value::Number(min), Value::Number(max)],
            kwargs: vec![("decimals".to_string(), Value::Number(decimals as f64))],
            line: 0,
            col: 0,
        },
        inline_comment: None,
        reason: format!("numeric ({min} … {max}, {decimals} decimals)"),
    })
}

fn rule_low_cardinality(col: &HarvestColumn) -> Option<ColumnInference> {
    if !matches!(col.pg_type, PgType::Text) {
        return None;
    }
    if col.sample.rows_examined < LOW_CARDINALITY_MIN_ROWS {
        return None;
    }
    if col.sample.distinct_count == 0 || col.sample.distinct_count > LOW_CARDINALITY_DISTINCT {
        return None;
    }
    let values: Vec<Value> = col.sample.values.iter().cloned().map(Value::String).collect();
    if values.is_empty() {
        return None;
    }
    Some(ColumnInference {
        call: Call {
            function: "randomChoice".to_string(),
            positional: values,
            kwargs: Vec::new(),
            line: 0,
            col: 0,
        },
        inline_comment: Some(format!(
            "{} distinct values in sample",
            col.sample.distinct_count
        )),
        reason: format!("{} distinct values across sample", col.sample.distinct_count),
    })
}

fn rule_text_pattern(col: &HarvestColumn) -> Option<ColumnInference> {
    if !matches!(col.pg_type, PgType::Text) {
        return None;
    }
    if col.sample.values.is_empty() {
        return None;
    }

    if pattern_fraction(col, is_email) >= PATTERN_MATCH_THRESHOLD {
        return Some(ColumnInference {
            call: Call {
                function: "randomEmail".to_string(),
                positional: Vec::new(),
                kwargs: Vec::new(),
                line: 0,
                col: 0,
            },
            inline_comment: None,
            reason: "≥90% of sample matches email pattern".to_string(),
        });
    }
    if pattern_fraction(col, is_iso_date) >= PATTERN_MATCH_THRESHOLD {
        // Compute observed min/max as strings; they're already ISO so
        // lexicographic min/max == chronological min/max.
        let mut vals: Vec<&String> = col.sample.values.iter().collect();
        vals.sort();
        let lo = vals.first().map(|s| s.as_str()).unwrap_or("2000-01-01");
        let hi = vals.last().map(|s| s.as_str()).unwrap_or("2024-12-31");
        return Some(ColumnInference {
            call: random_date_call(lo, hi),
            inline_comment: None,
            reason: "≥90% of sample matches ISO date pattern".to_string(),
        });
    }
    if pattern_fraction(col, is_name_shape) >= PATTERN_MATCH_THRESHOLD {
        return Some(ColumnInference {
            call: Call {
                function: "randomName".to_string(),
                positional: Vec::new(),
                kwargs: Vec::new(),
                line: 0,
                col: 0,
            },
            inline_comment: None,
            reason: "≥90% of sample matches `first last` name pattern".to_string(),
        });
    }
    if pattern_fraction(col, is_single_word) >= PATTERN_MATCH_THRESHOLD {
        return Some(ColumnInference {
            call: Call {
                function: "randomWord".to_string(),
                positional: Vec::new(),
                kwargs: Vec::new(),
                line: 0,
                col: 0,
            },
            inline_comment: None,
            reason: "≥90% of sample is a single short token".to_string(),
        });
    }
    None
}

fn rule_fallback(col: &HarvestColumn) -> ColumnInference {
    let (type_label, message) = match &col.pg_type {
        PgType::Other(name) => (
            name.clone(),
            format!("TODO: column type `{name}` not auto-detected; falling back to randomWord()"),
        ),
        PgType::Text => (
            "text".to_string(),
            "TODO: no pattern matched and not low-cardinality; falling back to randomWord()"
                .to_string(),
        ),
        other => (
            format!("{other:?}"),
            format!("TODO: column kind `{other:?}` had no rule; falling back to randomWord()"),
        ),
    };
    ColumnInference {
        call: fallback_call(),
        inline_comment: Some(message),
        reason: format!("no rule matched; fallback for `{type_label}`"),
    }
}

fn fallback_call() -> Call {
    Call {
        function: "randomWord".to_string(),
        positional: Vec::new(),
        kwargs: Vec::new(),
        line: 0,
        col: 0,
    }
}

fn ref_call(table: &str, col: &str) -> Call {
    Call {
        function: "ref".to_string(),
        positional: vec![Value::ColumnRef {
            table: table.to_string(),
            column: col.to_string(),
        }],
        kwargs: Vec::new(),
        line: 0,
        col: 0,
    }
}

fn pattern_fraction(col: &HarvestColumn, p: impl Fn(&str) -> bool) -> f64 {
    if col.sample.values.is_empty() {
        return 0.0;
    }
    let matches = col.sample.values.iter().filter(|v| p(v)).count();
    matches as f64 / col.sample.values.len() as f64
}

// ---- pattern predicates ----------------------------------------------------

fn is_email(s: &str) -> bool {
    // Cheap structural check — same idea as the spec regex but without
    // pulling regex into the inference hot path. Three rules:
    //   * has exactly one `@`
    //   * non-empty local part with no whitespace
    //   * domain has a dot, non-empty halves, no whitespace
    let mut parts = s.split('@');
    let local = match parts.next() {
        Some(l) => l,
        None => return false,
    };
    let domain = match parts.next() {
        Some(d) => d,
        None => return false,
    };
    if parts.next().is_some() {
        return false;
    }
    if local.is_empty() || domain.is_empty() {
        return false;
    }
    if local.chars().any(|c| c.is_whitespace()) || domain.chars().any(|c| c.is_whitespace()) {
        return false;
    }
    match domain.rfind('.') {
        Some(i) if i > 0 && i < domain.len() - 1 => true,
        _ => false,
    }
}

fn is_uuid(s: &str) -> bool {
    // 8-4-4-4-12 hex with dashes. Accept any hex case.
    if s.len() != 36 {
        return false;
    }
    let bytes = s.as_bytes();
    for (i, b) in bytes.iter().enumerate() {
        match i {
            8 | 13 | 18 | 23 => {
                if *b != b'-' {
                    return false;
                }
            }
            _ => {
                if !b.is_ascii_hexdigit() {
                    return false;
                }
            }
        }
    }
    true
}

fn is_iso_date(s: &str) -> bool {
    // YYYY-MM-DD. We don't validate month/day ranges — that's overkill for
    // a pattern detector.
    if s.len() != 10 {
        return false;
    }
    let bytes = s.as_bytes();
    bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[..4].iter().all(|b| b.is_ascii_digit())
        && bytes[5..7].iter().all(|b| b.is_ascii_digit())
        && bytes[8..10].iter().all(|b| b.is_ascii_digit())
}

fn is_name_shape(s: &str) -> bool {
    // Two whitespace-separated tokens, each ≥2 alphabetic chars (Unicode-
    // letter, not just ASCII). Catches "Alice Smith" but rejects "alice@example".
    let tokens: Vec<&str> = s.split_whitespace().collect();
    if tokens.len() != 2 {
        return false;
    }
    tokens.iter().all(|t| t.len() >= 2 && t.chars().all(|c| c.is_alphabetic()))
}

fn is_single_word(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() || s.len() > 24 {
        return false;
    }
    s.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-')
}

fn function_label(call: &Call) -> String {
    match call.function.as_str() {
        "ref" => match call.positional.first() {
            Some(Value::ColumnRef { table, column }) => format!("ref({table}.{column})"),
            _ => "ref(?)".to_string(),
        },
        other => format!("{other}(…)"),
    }
}

// `BTreeSet` import kept so future per-table extensions can include
// referenced-column sets without re-importing.
#[allow(dead_code)]
fn _unused_marker(_: BTreeSet<&str>) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harvest::model::{
        ColumnSample, ForeignKey, GeomMeta, HarvestColumn, NumericStats, SamplingStrategy,
    };

    fn table(name: &str, cols: Vec<HarvestColumn>) -> HarvestTable {
        HarvestTable {
            name: name.to_string(),
            columns: cols,
            primary_key: vec![],
            foreign_keys: vec![],
            estimated_rows: 100,
            sampling: SamplingStrategy::PkOrdered,
        }
    }

    fn col_int_pk() -> HarvestColumn {
        let mut c = HarvestColumn::bare("id", PgType::Integer { width: 4 });
        c.identity = true;
        c
    }

    fn integer_col(name: &str, min: i64, max: i64) -> HarvestColumn {
        let mut c = HarvestColumn::bare(name, PgType::Integer { width: 4 });
        c.sample = ColumnSample {
            rows_examined: 100,
            distinct_count: 100,
            stats: NumericStats {
                min: Some(min.to_string()),
                max: Some(max.to_string()),
                ..Default::default()
            },
            ..Default::default()
        };
        c
    }

    fn text_col(name: &str, values: Vec<&str>) -> HarvestColumn {
        let mut c = HarvestColumn::bare(name, PgType::Text);
        let mut sorted: Vec<String> = values.iter().map(|s| s.to_string()).collect();
        sorted.sort();
        let n = sorted.len();
        c.sample = ColumnSample {
            rows_examined: 100,
            distinct_count: n,
            values: sorted,
            ..Default::default()
        };
        c
    }

    fn schema_with(tables: Vec<HarvestTable>) -> BTreeMap<&'static str, &'static HarvestTable> {
        // For tests we leak the tables so the map can hold &'static — only
        // used in unit tests, not on the hot path.
        let mut map = BTreeMap::new();
        for t in tables {
            let leaked: &'static HarvestTable = Box::leak(Box::new(t));
            map.insert(leaked.name.as_str(), leaked);
        }
        map
    }

    #[test]
    fn declared_fk_beats_sequence() {
        // Junction-table column: PK + FK. FK rule wins.
        let mut child = HarvestColumn::bare("user_id", PgType::Integer { width: 4 });
        child.identity = false;
        let mut t = table("user_roles", vec![child]);
        t.primary_key = vec!["user_id".to_string()];
        t.foreign_keys = vec![ForeignKey {
            constraint_name: "fk_user".to_string(),
            columns: vec!["user_id".to_string()],
            ref_table: "users".to_string(),
            ref_columns: vec!["id".to_string()],
        }];
        let schema = BTreeMap::new();
        let r = infer_column(&t, &t.columns[0], &schema);
        assert_eq!(r.call.function, "ref");
    }

    #[test]
    fn integer_pk_becomes_sequence() {
        let mut t = table("users", vec![col_int_pk()]);
        t.primary_key = vec!["id".to_string()];
        let schema = BTreeMap::new();
        let r = infer_column(&t, &t.columns[0], &schema);
        assert_eq!(r.call.function, "sequence");
    }

    #[test]
    fn heuristic_fk_matches_pluralised_table() {
        let users = table("users", vec![col_int_pk()]);
        let schema = schema_with(vec![users]);

        let mut order = HarvestColumn::bare("user_id", PgType::Integer { width: 4 });
        order.sample = ColumnSample {
            rows_examined: 100,
            distinct_count: 50,
            ..Default::default()
        };
        let orders = table("orders", vec![order]);
        let r = infer_column(&orders, &orders.columns[0], &schema);
        assert_eq!(r.call.function, "ref");
        match &r.call.positional[0] {
            Value::ColumnRef { table, column } => {
                assert_eq!(table, "users");
                assert_eq!(column, "id");
            }
            _ => panic!("expected ColumnRef"),
        }
    }

    #[test]
    fn email_pattern_wins_over_low_cardinality() {
        // Below the low-cardinality min-rows threshold ALSO, but here we
        // simulate a sample of 100 distinct emails — too many for low
        // cardinality so the pattern rule should fire.
        let emails: Vec<String> = (0..30)
            .map(|i| format!("user{i}@example.com"))
            .collect();
        let mut c = HarvestColumn::bare("email", PgType::Text);
        c.sample = ColumnSample {
            rows_examined: 100,
            distinct_count: emails.len(),
            values: emails,
            ..Default::default()
        };
        let t = table("users", vec![c]);
        let schema = BTreeMap::new();
        let r = infer_column(&t, &t.columns[0], &schema);
        assert_eq!(r.call.function, "randomEmail");
    }

    #[test]
    fn uuid_native_type_emits_random_uuid() {
        let c = HarvestColumn::bare("id", PgType::Uuid);
        let t = table("docs", vec![c]);
        let schema = BTreeMap::new();
        let r = infer_column(&t, &t.columns[0], &schema);
        assert_eq!(r.call.function, "randomUuid");
    }

    #[test]
    fn boolean_uses_observed_true_fraction() {
        let mut c = HarvestColumn::bare("active", PgType::Boolean);
        c.sample = ColumnSample {
            rows_examined: 100,
            stats: NumericStats {
                true_fraction: Some(0.87),
                ..Default::default()
            },
            ..Default::default()
        };
        let t = table("users", vec![c]);
        let schema = BTreeMap::new();
        let r = infer_column(&t, &t.columns[0], &schema);
        assert_eq!(r.call.function, "randomBool");
        assert!(matches!(
            r.call.kwargs.iter().find(|(k, _)| k == "weight"),
            Some((_, Value::Number(v))) if (*v - 0.87).abs() < 1e-9
        ));
    }

    #[test]
    fn timestamp_downgrades_to_date_with_todo() {
        let mut c = HarvestColumn::bare("created_at", PgType::Timestamp { with_tz: true });
        c.sample = ColumnSample {
            rows_examined: 100,
            stats: NumericStats {
                min: Some("2022-01-01".to_string()),
                max: Some("2026-05-13".to_string()),
                ..Default::default()
            },
            ..Default::default()
        };
        let t = table("orders", vec![c]);
        let schema = BTreeMap::new();
        let r = infer_column(&t, &t.columns[0], &schema);
        assert_eq!(r.call.function, "randomDate");
        assert!(r
            .inline_comment
            .as_deref()
            .unwrap_or("")
            .contains("TODO: source is timestamp"));
    }

    #[test]
    fn low_cardinality_text_emits_random_choice_sorted() {
        let c = text_col(
            "status",
            vec!["pending", "shipped", "cancelled", "delivered"],
        );
        let t = table("orders", vec![c]);
        let schema = BTreeMap::new();
        let r = infer_column(&t, &t.columns[0], &schema);
        assert_eq!(r.call.function, "randomChoice");
        let values: Vec<&str> = r
            .call
            .positional
            .iter()
            .filter_map(|v| match v {
                Value::String(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(values, vec!["cancelled", "delivered", "pending", "shipped"]);
    }

    #[test]
    fn geometry_point_uses_observed_bbox() {
        let mut c = HarvestColumn::bare("loc", PgType::Geometry);
        c.geom_meta = Some(GeomMeta {
            declared_type: Some("POINT".to_string()),
            observed_types: {
                let mut s = BTreeSet::new();
                s.insert("POINT".to_string());
                s
            },
            srid: Some(4326),
            bbox: Some([3.3140, 50.7510, 7.2275, 53.5550]),
            avg_segments: None,
            avg_vertices: None,
        });
        c.sample.rows_examined = 8932;
        let t = table("orders", vec![c]);
        let schema = BTreeMap::new();
        let r = infer_column(&t, &t.columns[0], &schema);
        assert_eq!(r.call.function, "randomPoint");
        let bbox_kw = r.call.kwargs.iter().find(|(k, _)| k == "bbox").unwrap();
        match &bbox_kw.1 {
            Value::Array(items) => {
                assert_eq!(items.len(), 4);
                if let Value::Number(n) = &items[0] {
                    assert!((n - 3.3140).abs() < 1e-9);
                } else {
                    panic!("bbox[0] not a number");
                }
            }
            _ => panic!("bbox not an array"),
        }
    }

    #[test]
    fn unrecognised_type_falls_through_to_random_word() {
        let c = HarvestColumn::bare("payload", PgType::Other("jsonb".to_string()));
        let t = table("events", vec![c]);
        let schema = BTreeMap::new();
        let r = infer_column(&t, &t.columns[0], &schema);
        assert_eq!(r.call.function, "randomWord");
        assert!(r
            .inline_comment
            .as_deref()
            .unwrap_or("")
            .contains("TODO"));
    }

    #[test]
    fn nullable_column_gets_null_annotation() {
        let mut c = integer_col("score", 1, 100);
        c.is_nullable = true;
        c.sample.null_count = 17;
        c.sample.rows_examined = 100;
        let t = table("scores", vec![c]);
        let schema = BTreeMap::new();
        let r = infer_column(&t, &t.columns[0], &schema);
        assert!(r
            .inline_comment
            .as_deref()
            .unwrap_or("")
            .contains("source allows NULL"));
    }

    #[test]
    fn email_predicate_basic() {
        assert!(is_email("user@example.com"));
        assert!(!is_email("nope@nodot"));
        assert!(!is_email("two@@signs.com"));
        assert!(!is_email("@nostart.com"));
    }

    #[test]
    fn uuid_predicate_format() {
        assert!(is_uuid("123e4567-e89b-12d3-a456-426614174000"));
        assert!(!is_uuid("123e4567-e89b-12d3-a456-42661417400")); // too short
        assert!(!is_uuid("123e4567e89b12d3a456426614174000")); // no dashes
    }

    #[test]
    fn date_predicate_format() {
        assert!(is_iso_date("2020-01-31"));
        assert!(!is_iso_date("2020/01/31"));
        assert!(!is_iso_date("20-01-31"));
    }

    #[test]
    fn name_predicate_shape() {
        assert!(is_name_shape("Alice Smith"));
        assert!(!is_name_shape("alice@example"));
        assert!(!is_name_shape("Just"));
        assert!(!is_name_shape("Three Word Name"));
    }

    #[test]
    fn determinism_round_trip_same_input_same_output() {
        let mut c = HarvestColumn::bare("status", PgType::Text);
        c.sample = ColumnSample {
            rows_examined: 80,
            distinct_count: 3,
            values: vec!["cancelled".into(), "pending".into(), "shipped".into()],
            ..Default::default()
        };
        let t = table("orders", vec![c.clone()]);
        let schema = BTreeMap::new();
        let r1 = infer_column(&t, &t.columns[0], &schema);
        let r2 = infer_column(&t, &t.columns[0], &schema);
        assert_eq!(r1.call, r2.call);
        assert_eq!(r1.inline_comment, r2.inline_comment);
    }
}
