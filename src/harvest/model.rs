//! Internal IR for the harvest pipeline. These types are always compiled,
//! even with the `harvest` feature disabled, so that downstream crates (and
//! tests) can construct fixture values without pulling in postgres.

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};

/// Top-level result of "look at the source DB". Inference reads this;
/// emission reads the post-inference [`InferenceOutput`] (see infer.rs).
#[derive(Debug, Clone)]
pub struct HarvestSchema {
    pub source: SourceInfo,
    pub tables: Vec<HarvestTable>,
    /// PostGIS extension present in the source DB. Geometry inference only
    /// runs when this is true.
    pub geometry_supported: bool,
}

#[derive(Debug, Clone)]
pub struct SourceInfo {
    pub database: String,
    pub schema: String,
    pub harvested_at: DateTime<Utc>,
    /// Raw invocation, for the file's header comment. Sanitised: we drop
    /// the password component of the connection URL.
    pub invocation: String,
}

#[derive(Debug, Clone)]
pub struct HarvestTable {
    pub name: String,
    pub columns: Vec<HarvestColumn>,
    pub primary_key: Vec<String>,
    pub foreign_keys: Vec<ForeignKey>,
    pub estimated_rows: u64,
    pub sampling: SamplingStrategy,
}

#[derive(Debug, Clone)]
pub struct ForeignKey {
    #[allow(dead_code)]
    pub constraint_name: String,
    pub columns: Vec<String>,
    pub ref_table: String,
    pub ref_columns: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct HarvestColumn {
    pub name: String,
    pub pg_type: PgType,
    pub is_nullable: bool,
    pub default: ColumnDefault,
    /// `GENERATED ALWAYS|BY DEFAULT AS IDENTITY` — distinct from a default
    /// of nextval('...'). Either form should produce `sequence`.
    pub identity: bool,
    pub sample: ColumnSample,
    /// Populated only for geometry/geography columns and only when PostGIS
    /// is detected at introspect time.
    pub geom_meta: Option<GeomMeta>,
}

/// Coarse type bucket. Carries enough to drive type-based inference;
/// keeps the rare/exotic types as `Other(String)` so the fallback rule
/// has something to print in its TODO comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PgType {
    Integer { width: u8 },
    Real { is_double: bool, scale: Option<u8> },
    Text,
    Boolean,
    Date,
    Timestamp { with_tz: bool },
    Uuid,
    Geometry,
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColumnDefault {
    None,
    /// `nextval('...')` or attached sequence.
    Sequence,
    /// Anything else — kept verbatim for the inference reasoning string.
    Expr(String),
}

#[derive(Debug, Clone, Default)]
pub struct ColumnSample {
    pub rows_examined: usize,
    pub null_count: usize,
    pub distinct_count: usize,
    /// Distinct observed values, formatted as strings. Sorted alphabetically
    /// so harvest output is deterministic across runs.
    ///
    /// Capped at `MAX_DISTINCT_VALUES` (see infer.rs); beyond that we keep
    /// the count but stop recording values (low-cardinality rule won't fire
    /// past the cap anyway).
    pub values: Vec<String>,
    pub stats: NumericStats,
}

#[derive(Debug, Clone, Default)]
pub struct NumericStats {
    /// Server-side `MIN(col)` formatted as Postgres returned it. `None`
    /// when the column isn't numeric/date or the table is empty.
    pub min: Option<String>,
    pub max: Option<String>,
    /// Maximum decimal places observed in the sample. For columns declared
    /// `numeric(p, s)` we prefer the typmod scale (PgType::Real.scale).
    pub observed_decimals: Option<u8>,
    /// Observed fraction of `true` values in the sample, for booleans.
    pub true_fraction: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct GeomMeta {
    /// Declared type from `geometry_columns`, e.g. `"POINT"`. Empty string
    /// or `"GEOMETRY"` means "polymorphic geometry"; we then trust the
    /// observed types.
    pub declared_type: Option<String>,
    pub observed_types: BTreeSet<String>,
    pub srid: Option<i32>,
    /// `[minLon, minLat, maxLon, maxLat]`. `None` when the table is empty
    /// or every sampled value is null.
    pub bbox: Option<[f64; 4]>,
    pub avg_segments: Option<u32>,
    pub avg_vertices: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SamplingStrategy {
    PkOrdered,
    CtidOrdered,
    /// Table is empty; no sampling done.
    NoSample,
}

impl HarvestColumn {
    /// Convenience for tests: empty sample, not nullable, no default.
    pub fn bare(name: impl Into<String>, ty: PgType) -> Self {
        Self {
            name: name.into(),
            pg_type: ty,
            is_nullable: false,
            default: ColumnDefault::None,
            identity: false,
            sample: ColumnSample::default(),
            geom_meta: None,
        }
    }
}

impl HarvestTable {
    /// Whether `col` is the (single-column) primary key of this table.
    pub fn is_single_col_pk(&self, col: &str) -> bool {
        self.primary_key.len() == 1 && self.primary_key[0] == col
    }

    /// Find the foreign key that covers `col` as its only child column.
    /// Composite FKs are ignored — Phase 4 emits one `ref()` per child
    /// column and can't represent multi-column FKs faithfully.
    pub fn single_col_fk_for<'a>(&'a self, col: &str) -> Option<&'a ForeignKey> {
        self.foreign_keys
            .iter()
            .find(|fk| fk.columns.len() == 1 && fk.columns[0] == col)
    }
}
