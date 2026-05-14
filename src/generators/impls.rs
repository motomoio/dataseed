//! Concrete generator implementations and the `bind` dispatcher.
//!
//! Each generator owns:
//!   * a `bind_*` function that validates the AST call and produces a
//!     `Box<dyn Generator>`;
//!   * a struct implementing `Generator::produce`.
//!
//! The validation is intentionally hand-rolled per generator because each
//! has a small, distinct set of rules. Centralising validation would make
//! the error messages worse without saving much code.

use std::sync::LazyLock;

use chrono::NaiveDate;
use rand::Rng;
use uuid::Uuid;

use crate::ast::{Call, Value};
use crate::error::SemanticError;
use crate::generators::spec;
use crate::generators::{Cell, Generator};
use crate::rng::SeedRng;

// ---------- bundled data --------------------------------------------------

static WORDS: LazyLock<Vec<&'static str>> = LazyLock::new(|| {
    include_str!("data/words.txt")
        .split_whitespace()
        .filter(|w| !w.is_empty())
        .collect()
});
static FIRST_NAMES: LazyLock<Vec<&'static str>> = LazyLock::new(|| {
    include_str!("data/first_names.txt")
        .split_whitespace()
        .filter(|w| !w.is_empty())
        .collect()
});
static LAST_NAMES: LazyLock<Vec<&'static str>> = LazyLock::new(|| {
    include_str!("data/last_names.txt")
        .split_whitespace()
        .filter(|w| !w.is_empty())
        .collect()
});

// ---------- dispatcher ----------------------------------------------------

pub fn bind(call: &Call) -> Result<Box<dyn Generator>, SemanticError> {
    match call.function.as_str() {
        "sequence" => bind_sequence(call),
        "randomBool" => bind_random_bool(call),
        "randomChoice" => bind_random_choice(call),
        "randomDate" => bind_random_date(call),
        "randomEmail" => bind_random_email(call),
        "randomInt" => bind_random_int(call),
        "randomName" => bind_random_name(call),
        "randomRealNumber" => bind_random_real(call),
        "randomUuid" => bind_random_uuid(call),
        "randomWord" => bind_random_word(call),
        // Phase 2 — geospatial
        "randomPoint" => super::geo::bind_random_point(call),
        "randomPointNear" => super::geo::bind_random_point_near(call),
        "randomLineString" => super::geo::bind_random_line_string(call),
        "randomPolygon" => super::geo::bind_random_polygon(call),
        "randomBbox" => super::geo::bind_random_bbox(call),
        // Phase 3 — relations
        "ref" => bind_ref(call),
        other => Err(SemanticError::UnknownFunction {
            line: call.line,
            col: call.col,
            name: other.to_string(),
            suggestion: None,
        }),
    }
}

// ---------- helpers -------------------------------------------------------

fn no_positional(call: &Call) -> Result<(), SemanticError> {
    if call.positional.is_empty() {
        Ok(())
    } else {
        Err(SemanticError::WrongArity {
            line: call.line,
            col: call.col,
            function: call.function.clone(),
            expected: "0 positional arguments".to_string(),
            got: call.positional.len(),
        })
    }
}

fn require_no_kwargs_except(
    call: &Call,
    allowed: &'static [&'static str],
) -> Result<(), SemanticError> {
    for (k, _) in &call.kwargs {
        if !allowed.contains(&k.as_str()) {
            return Err(SemanticError::UnknownKwarg {
                line: call.line,
                col: call.col,
                function: call.function.clone(),
                name: k.clone(),
                allowed: allowed.to_vec(),
            });
        }
    }
    Ok(())
}

fn find_kwarg<'a>(call: &'a Call, name: &str) -> Option<&'a Value> {
    call.kwargs.iter().find(|(k, _)| k == name).map(|(_, v)| v)
}

fn expect_number(call: &Call, arg: &str, v: &Value) -> Result<f64, SemanticError> {
    match v {
        Value::Number(n) => Ok(*n),
        other => Err(SemanticError::TypeMismatch {
            line: call.line,
            col: call.col,
            function: call.function.clone(),
            arg: arg.to_string(),
            expected: "number",
            got: other.type_name(),
        }),
    }
}

fn expect_integer(call: &Call, arg: &str, v: &Value) -> Result<i64, SemanticError> {
    match v {
        Value::Number(n) => {
            if n.fract() != 0.0 || !n.is_finite() {
                return Err(SemanticError::TypeMismatch {
                    line: call.line,
                    col: call.col,
                    function: call.function.clone(),
                    arg: arg.to_string(),
                    expected: "integer",
                    got: "non-integer number",
                });
            }
            if *n < i64::MIN as f64 || *n > i64::MAX as f64 {
                return Err(SemanticError::InvalidArgValue {
                    line: call.line,
                    col: call.col,
                    function: call.function.clone(),
                    arg: arg.to_string(),
                    reason: "value out of i64 range".to_string(),
                });
            }
            Ok(*n as i64)
        }
        other => Err(SemanticError::TypeMismatch {
            line: call.line,
            col: call.col,
            function: call.function.clone(),
            arg: arg.to_string(),
            expected: "integer",
            got: other.type_name(),
        }),
    }
}

fn expect_string<'a>(call: &Call, arg: &str, v: &'a Value) -> Result<&'a str, SemanticError> {
    match v {
        Value::String(s) => Ok(s),
        other => Err(SemanticError::TypeMismatch {
            line: call.line,
            col: call.col,
            function: call.function.clone(),
            arg: arg.to_string(),
            expected: "string",
            got: other.type_name(),
        }),
    }
}

// ---------- sequence ------------------------------------------------------

pub fn bind_sequence(call: &Call) -> Result<Box<dyn Generator>, SemanticError> {
    no_positional(call)?;
    require_no_kwargs_except(call, &["start"])?;
    let start = match find_kwarg(call, "start") {
        Some(v) => expect_integer(call, "start", v)?,
        None => 1,
    };
    Ok(Box::new(Sequence { start }))
}

struct Sequence {
    start: i64,
}

impl Generator for Sequence {
    fn produce(&mut self, _rng: &mut SeedRng, ctx: &crate::output::RowCtx) -> Cell {
        // Use checked_add to avoid silent wrap on absurdly large generates.
        let value = (self.start as i128) + (ctx.row as i128);
        Cell::Integer(value.clamp(i64::MIN as i128, i64::MAX as i128) as i64)
    }
}

// ---------- randomInt -----------------------------------------------------

pub fn bind_random_int(call: &Call) -> Result<Box<dyn Generator>, SemanticError> {
    if call.positional.len() != 2 {
        return Err(SemanticError::WrongArity {
            line: call.line,
            col: call.col,
            function: call.function.clone(),
            expected: "exactly 2 positional arguments (min, max)".to_string(),
            got: call.positional.len(),
        });
    }
    require_no_kwargs_except(call, &[])?;
    let min = expect_integer(call, "min", &call.positional[0])?;
    let max = expect_integer(call, "max", &call.positional[1])?;
    if min > max {
        return Err(SemanticError::InvalidArgValue {
            line: call.line,
            col: call.col,
            function: call.function.clone(),
            arg: "min".into(),
            reason: format!("min ({min}) must be <= max ({max})"),
        });
    }
    Ok(Box::new(RandomInt { min, max }))
}

struct RandomInt {
    min: i64,
    max: i64,
}

impl Generator for RandomInt {
    fn produce(&mut self, rng: &mut SeedRng, _ctx: &crate::output::RowCtx) -> Cell {
        Cell::Integer(rng.gen_range_i64(self.min, self.max))
    }
}

// ---------- randomRealNumber ---------------------------------------------

pub fn bind_random_real(call: &Call) -> Result<Box<dyn Generator>, SemanticError> {
    if call.positional.len() != 2 {
        return Err(SemanticError::WrongArity {
            line: call.line,
            col: call.col,
            function: call.function.clone(),
            expected: "exactly 2 positional arguments (min, max)".to_string(),
            got: call.positional.len(),
        });
    }
    require_no_kwargs_except(call, &["decimals"])?;
    let min = expect_number(call, "min", &call.positional[0])?;
    let max = expect_number(call, "max", &call.positional[1])?;
    if min > max {
        return Err(SemanticError::InvalidArgValue {
            line: call.line,
            col: call.col,
            function: call.function.clone(),
            arg: "min".into(),
            reason: format!("min ({min}) must be <= max ({max})"),
        });
    }
    let decimals = match find_kwarg(call, "decimals") {
        Some(v) => expect_integer(call, "decimals", v)?,
        None => 2,
    };
    if !(0..=15).contains(&decimals) {
        return Err(SemanticError::InvalidArgValue {
            line: call.line,
            col: call.col,
            function: call.function.clone(),
            arg: "decimals".into(),
            reason: format!("must be between 0 and 15, got {decimals}"),
        });
    }
    Ok(Box::new(RandomReal { min, max, decimals: decimals as u32 }))
}

struct RandomReal {
    min: f64,
    max: f64,
    decimals: u32,
}

impl Generator for RandomReal {
    fn produce(&mut self, rng: &mut SeedRng, _ctx: &crate::output::RowCtx) -> Cell {
        let raw = rng.gen_range_f64(self.min, self.max);
        let factor = 10f64.powi(self.decimals as i32);
        // round-half-to-even would be more correct, but standard `round` is
        // adequate for synthetic test data and matches user expectations.
        let rounded = (raw * factor).round() / factor;
        Cell::Real(rounded)
    }
}

// ---------- randomBool ----------------------------------------------------

pub fn bind_random_bool(call: &Call) -> Result<Box<dyn Generator>, SemanticError> {
    no_positional(call)?;
    require_no_kwargs_except(call, &["weight"])?;
    let weight = match find_kwarg(call, "weight") {
        Some(v) => expect_number(call, "weight", v)?,
        None => 0.5,
    };
    if !(0.0..=1.0).contains(&weight) {
        return Err(SemanticError::InvalidArgValue {
            line: call.line,
            col: call.col,
            function: call.function.clone(),
            arg: "weight".into(),
            reason: format!("must be between 0.0 and 1.0, got {weight}"),
        });
    }
    Ok(Box::new(RandomBool { weight }))
}

struct RandomBool {
    weight: f64,
}

impl Generator for RandomBool {
    fn produce(&mut self, rng: &mut SeedRng, _ctx: &crate::output::RowCtx) -> Cell {
        Cell::Bool(rng.gen_bool(self.weight))
    }
}

// ---------- randomChoice --------------------------------------------------

pub fn bind_random_choice(call: &Call) -> Result<Box<dyn Generator>, SemanticError> {
    if call.positional.is_empty() {
        return Err(SemanticError::WrongArity {
            line: call.line,
            col: call.col,
            function: call.function.clone(),
            expected: "at least 1 positional argument".to_string(),
            got: 0,
        });
    }
    require_no_kwargs_except(call, &[])?;
    // The grammar permits a mix of literal types; we convert at bind time
    // so produce() is a flat copy. Use String for everything textual,
    // preserve numeric/bool typing for SQL emission.
    let choices: Result<Vec<Cell>, SemanticError> = call
        .positional
        .iter()
        .enumerate()
        .map(|(i, v)| match v {
            Value::Number(n) => Ok(
                if n.fract() == 0.0 && n.is_finite() && *n >= i64::MIN as f64 && *n <= i64::MAX as f64 {
                    Cell::Integer(*n as i64)
                } else {
                    Cell::Real(*n)
                },
            ),
            Value::String(s) => Ok(Cell::Text(s.clone())),
            Value::Bool(b) => Ok(Cell::Bool(*b)),
            Value::Array(_) => Err(SemanticError::TypeMismatch {
                line: call.line,
                col: call.col,
                function: call.function.clone(),
                arg: format!("choice #{}", i + 1),
                expected: "scalar (number, string, or boolean)",
                got: "array",
            }),
            Value::ColumnRef { .. } => Err(SemanticError::TypeMismatch {
                line: call.line,
                col: call.col,
                function: call.function.clone(),
                arg: format!("choice #{}", i + 1),
                expected: "scalar (number, string, or boolean)",
                got: "column_reference",
            }),
            Value::Range { .. } => Err(SemanticError::TypeMismatch {
                line: call.line,
                col: call.col,
                function: call.function.clone(),
                arg: format!("choice #{}", i + 1),
                expected: "scalar (number, string, or boolean)",
                got: "range",
            }),
        })
        .collect();
    Ok(Box::new(RandomChoice { choices: choices? }))
}

struct RandomChoice {
    choices: Vec<Cell>,
}

impl Generator for RandomChoice {
    fn produce(&mut self, rng: &mut SeedRng, _ctx: &crate::output::RowCtx) -> Cell {
        let idx = rng.pick_index(self.choices.len());
        self.choices[idx].clone()
    }
}

// ---------- randomWord ----------------------------------------------------

pub fn bind_random_word(call: &Call) -> Result<Box<dyn Generator>, SemanticError> {
    no_positional(call)?;
    require_no_kwargs_except(call, &[])?;
    Ok(Box::new(RandomWord))
}

struct RandomWord;

impl Generator for RandomWord {
    fn produce(&mut self, rng: &mut SeedRng, _ctx: &crate::output::RowCtx) -> Cell {
        let idx = rng.pick_index(WORDS.len());
        Cell::Text(WORDS[idx].to_string())
    }
}

// ---------- randomName ----------------------------------------------------

pub fn bind_random_name(call: &Call) -> Result<Box<dyn Generator>, SemanticError> {
    no_positional(call)?;
    require_no_kwargs_except(call, &[])?;
    Ok(Box::new(RandomName))
}

struct RandomName;

impl Generator for RandomName {
    fn produce(&mut self, rng: &mut SeedRng, _ctx: &crate::output::RowCtx) -> Cell {
        let first = FIRST_NAMES[rng.pick_index(FIRST_NAMES.len())];
        let last = LAST_NAMES[rng.pick_index(LAST_NAMES.len())];
        Cell::Text(format!("{first} {last}"))
    }
}

// ---------- randomEmail ---------------------------------------------------

pub fn bind_random_email(call: &Call) -> Result<Box<dyn Generator>, SemanticError> {
    no_positional(call)?;
    require_no_kwargs_except(call, &[])?;
    Ok(Box::new(RandomEmail))
}

struct RandomEmail;

impl Generator for RandomEmail {
    fn produce(&mut self, rng: &mut SeedRng, _ctx: &crate::output::RowCtx) -> Cell {
        let first = FIRST_NAMES[rng.pick_index(FIRST_NAMES.len())].to_lowercase();
        let last = LAST_NAMES[rng.pick_index(LAST_NAMES.len())].to_lowercase();
        // Strip apostrophes — `o'brien` is fine but `o\'brien` in SQL gets
        // confusing. Use a plain ASCII identifier on the local part.
        let local_first: String = first.chars().filter(|c| c.is_ascii_alphabetic()).collect();
        let local_last: String = last.chars().filter(|c| c.is_ascii_alphabetic()).collect();
        Cell::Text(format!("{local_first}.{local_last}@example.com"))
    }
}

// ---------- randomDate ----------------------------------------------------

pub fn bind_random_date(call: &Call) -> Result<Box<dyn Generator>, SemanticError> {
    if call.positional.len() != 2 {
        return Err(SemanticError::WrongArity {
            line: call.line,
            col: call.col,
            function: call.function.clone(),
            expected: "exactly 2 positional arguments (start, end) as ISO dates".to_string(),
            got: call.positional.len(),
        });
    }
    require_no_kwargs_except(call, &[])?;
    let start_str = expect_string(call, "start", &call.positional[0])?;
    let end_str = expect_string(call, "end", &call.positional[1])?;
    let start = parse_iso_date(call, "start", start_str)?;
    let end = parse_iso_date(call, "end", end_str)?;
    if start > end {
        return Err(SemanticError::InvalidArgValue {
            line: call.line,
            col: call.col,
            function: call.function.clone(),
            arg: "start".into(),
            reason: format!("start ({start}) must be <= end ({end})"),
        });
    }
    let span_days = (end - start).num_days();
    Ok(Box::new(RandomDate { start, span_days }))
}

fn parse_iso_date(call: &Call, arg: &str, s: &str) -> Result<NaiveDate, SemanticError> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(|_| SemanticError::InvalidArgValue {
        line: call.line,
        col: call.col,
        function: call.function.clone(),
        arg: arg.to_string(),
        reason: format!("expected ISO date `YYYY-MM-DD`, got `{s}`"),
    })
}

struct RandomDate {
    start: NaiveDate,
    span_days: i64,
}

impl Generator for RandomDate {
    fn produce(&mut self, rng: &mut SeedRng, _ctx: &crate::output::RowCtx) -> Cell {
        let offset = if self.span_days == 0 {
            0
        } else {
            rng.rng_mut().gen_range(0..=self.span_days)
        };
        let date = self.start + chrono::Duration::days(offset);
        Cell::Text(date.format("%Y-%m-%d").to_string())
    }
}

// ---------- randomUuid ----------------------------------------------------

pub fn bind_random_uuid(call: &Call) -> Result<Box<dyn Generator>, SemanticError> {
    no_positional(call)?;
    require_no_kwargs_except(call, &[])?;
    Ok(Box::new(RandomUuidGen))
}

struct RandomUuidGen;

impl Generator for RandomUuidGen {
    fn produce(&mut self, rng: &mut SeedRng, _ctx: &crate::output::RowCtx) -> Cell {
        // Build 16 bytes from the seeded RNG so the UUID is reproducible.
        // `uuid::Builder::from_random_bytes` clears version/variant bits and
        // sets them to RFC4122 v4 for us.
        let mut bytes = [0u8; 16];
        rng.rng_mut().fill(&mut bytes);
        Cell::Text(Uuid::from_bytes(set_v4(bytes)).to_string())
    }
}

fn set_v4(mut bytes: [u8; 16]) -> [u8; 16] {
    // version 4 (random) — high nibble of byte 6
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    // RFC4122 variant — top two bits of byte 8
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    bytes
}

// ---------- ref (Phase 3) -------------------------------------------------

pub fn bind_ref(call: &Call) -> Result<Box<dyn Generator>, SemanticError> {
    if call.positional.len() != 1 {
        return Err(SemanticError::WrongArity {
            line: call.line,
            col: call.col,
            function: call.function.clone(),
            expected: "exactly 1 positional argument (table.column)".into(),
            got: call.positional.len(),
        });
    }
    require_no_kwargs_except(call, &["per_parent"])?;
    let per_parent = match find_kwarg(call, "per_parent") {
        Some(Value::Range { lo, hi }) => {
            if *lo < 0 {
                return Err(SemanticError::InvalidArgValue {
                    line: call.line,
                    col: call.col,
                    function: call.function.clone(),
                    arg: "per_parent".into(),
                    reason: format!("range bounds must be >= 0, got {lo}..{hi}"),
                });
            }
            Some((*lo as u64, *hi as u64))
        }
        Some(other) => {
            return Err(SemanticError::TypeMismatch {
                line: call.line,
                col: call.col,
                function: call.function.clone(),
                arg: "per_parent".into(),
                expected: "range (N..M)",
                got: other.type_name(),
            });
        }
        None => None,
    };
    let (table, column) = match &call.positional[0] {
        Value::ColumnRef { table, column } => (table.clone(), column.clone()),
        other => {
            return Err(SemanticError::TypeMismatch {
                line: call.line,
                col: call.col,
                function: call.function.clone(),
                arg: "target".into(),
                expected: "column reference (table.column)",
                got: other.type_name(),
            });
        }
    };
    Ok(Box::new(RefGen { table, column, per_parent }))
}

struct RefGen {
    table: String,
    column: String,
    // per_parent is read by the engine (Task 1.4) to drive child-row quotas.
    // RefGen itself still does its current uniform pick when no quota is
    // active; the field is stored here so the catalog stays in sync.
    #[allow(dead_code)]
    per_parent: Option<(u64, u64)>,
}

impl Generator for RefGen {
    fn produce(&mut self, rng: &mut SeedRng, ctx: &crate::output::RowCtx) -> Cell {
        // Semantic checker has already verified the (table, column) exists.
        // If we reach here with no values, it's an engine-ordering bug: a
        // table was generated before its dependency.
        let values = ctx.pool.get(&self.table, &self.column).unwrap_or_else(|| {
            panic!(
                "ref({}.{}): pool empty — engine generated this table before its dependency",
                self.table, self.column
            )
        });
        if values.is_empty() {
            // Parent table has zero rows. There's nothing to draw from.
            // We could panic; instead we surface this with a clear runtime
            // panic that names the cause so the user can fix the parent's
            // `generate` count.
            panic!(
                "ref({}.{}): parent table has zero rows — set its `generate` count > 0",
                self.table, self.column
            );
        }
        let idx = match ctx.forced_parent {
            Some((pt, pc, parent_idx)) if pt == self.table && pc == self.column => {
                parent_idx % values.len()
            }
            _ => rng.pick_index(values.len()),
        };
        values[idx].clone()
    }
}

// ---------- spec sanity (debug-only) -------------------------------------

/// Cross-check: `spec::CATALOG` and the dispatch arms above must list the
/// same set of names. Caught by a unit test, not at runtime.
#[cfg(test)]
pub(super) fn dispatch_names() -> &'static [&'static str] {
    &[
        "sequence",
        "randomBool",
        "randomChoice",
        "randomDate",
        "randomEmail",
        "randomInt",
        "randomName",
        "randomRealNumber",
        "randomUuid",
        "randomWord",
        // Phase 2 — geospatial
        "randomPoint",
        "randomPointNear",
        "randomLineString",
        "randomPolygon",
        "randomBbox",
        // Phase 3 — relations
        "ref",
    ]
}

// Make spec::ArgSpec usable here without warnings if added later.
#[allow(dead_code)]
fn _spec_compile_check(_: &spec::ArgSpec) {}
