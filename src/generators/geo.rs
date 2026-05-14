//! Geospatial generators: `randomPoint`, `randomPointNear`, `randomLineString`,
//! `randomPolygon`, `randomBbox`.
//!
//! All coordinates are WGS84 longitude/latitude in decimal degrees. The
//! generators produce raw `f64` coordinates; rounding to display precision
//! happens at output time (see `crate::geometry::fmt_coord`).
//!
//! Determinism rules:
//! * Every random draw goes through `SeedRng`.
//! * `randomPointNear` uses `libm::cos` (pure-Rust software libm) so the
//!   single latitude-scale cosine call at bind time is bit-identical across
//!   targets. The other four generators are trig-free.

use crate::ast::{Call, Value};
use crate::error::SemanticError;
use crate::generators::{Cell, Generator};
use crate::geometry::Geometry;
use crate::rng::SeedRng;

// ---------- shared helpers ----------------------------------------------

fn require_array<'a>(
    call: &Call,
    arg: &str,
    v: &'a Value,
    expected_len: usize,
) -> Result<&'a [Value], SemanticError> {
    match v {
        Value::Array(items) if items.len() == expected_len => Ok(items.as_slice()),
        Value::Array(items) => Err(SemanticError::InvalidArgValue {
            line: call.line,
            col: call.col,
            function: call.function.clone(),
            arg: arg.to_string(),
            reason: format!(
                "expected array of length {expected_len}, got {}",
                items.len()
            ),
        }),
        other => Err(SemanticError::TypeMismatch {
            line: call.line,
            col: call.col,
            function: call.function.clone(),
            arg: arg.to_string(),
            expected: "array",
            got: other.type_name(),
        }),
    }
}

fn array_as_numbers(
    call: &Call,
    arg: &str,
    items: &[Value],
) -> Result<Vec<f64>, SemanticError> {
    items
        .iter()
        .enumerate()
        .map(|(i, v)| match v {
            Value::Number(n) => Ok(*n),
            other => Err(SemanticError::TypeMismatch {
                line: call.line,
                col: call.col,
                function: call.function.clone(),
                arg: format!("{arg}[{i}]"),
                expected: "number",
                got: other.type_name(),
            }),
        })
        .collect()
}

fn find_kwarg<'a>(call: &'a Call, name: &str) -> Option<&'a Value> {
    call.kwargs.iter().find(|(k, _)| k == name).map(|(_, v)| v)
}

fn require_kwargs_subset(
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
        Value::Number(n) if n.fract() == 0.0 && n.is_finite() => Ok(*n as i64),
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

#[derive(Debug, Clone, Copy)]
struct Bbox {
    min_lon: f64,
    min_lat: f64,
    max_lon: f64,
    max_lat: f64,
}

/// Validate a `[minLon, minLat, maxLon, maxLat]` array. Done at bind time
/// so bad input fails fast, before any row generation.
fn parse_bbox(
    call: &Call,
    arg: &str,
    v: &Value,
) -> Result<Bbox, SemanticError> {
    let items = require_array(call, arg, v, 4)?;
    let nums = array_as_numbers(call, arg, items)?;
    let bbox = Bbox {
        min_lon: nums[0],
        min_lat: nums[1],
        max_lon: nums[2],
        max_lat: nums[3],
    };
    if !(bbox.min_lon < bbox.max_lon) {
        return Err(SemanticError::InvalidArgValue {
            line: call.line,
            col: call.col,
            function: call.function.clone(),
            arg: arg.to_string(),
            reason: format!(
                "minLon ({}) must be < maxLon ({})",
                bbox.min_lon, bbox.max_lon
            ),
        });
    }
    if !(bbox.min_lat < bbox.max_lat) {
        return Err(SemanticError::InvalidArgValue {
            line: call.line,
            col: call.col,
            function: call.function.clone(),
            arg: arg.to_string(),
            reason: format!(
                "minLat ({}) must be < maxLat ({})",
                bbox.min_lat, bbox.max_lat
            ),
        });
    }
    if !(-180.0..=180.0).contains(&bbox.min_lon)
        || !(-180.0..=180.0).contains(&bbox.max_lon)
    {
        return Err(SemanticError::InvalidArgValue {
            line: call.line,
            col: call.col,
            function: call.function.clone(),
            arg: arg.to_string(),
            reason: "longitudes must lie in [-180, 180]".to_string(),
        });
    }
    if !(-90.0..=90.0).contains(&bbox.min_lat)
        || !(-90.0..=90.0).contains(&bbox.max_lat)
    {
        return Err(SemanticError::InvalidArgValue {
            line: call.line,
            col: call.col,
            function: call.function.clone(),
            arg: arg.to_string(),
            reason: "latitudes must lie in [-90, 90]".to_string(),
        });
    }
    Ok(bbox)
}

// ---------- randomPoint --------------------------------------------------

pub fn bind_random_point(call: &Call) -> Result<Box<dyn Generator>, SemanticError> {
    require_kwargs_subset(call, &["bbox"])?;
    if !call.positional.is_empty() {
        return Err(SemanticError::WrongArity {
            line: call.line,
            col: call.col,
            function: call.function.clone(),
            expected: "0 positional arguments".into(),
            got: call.positional.len(),
        });
    }
    let bbox_val = find_kwarg(call, "bbox").ok_or_else(|| SemanticError::MissingArg {
        line: call.line,
        col: call.col,
        function: call.function.clone(),
        arg: "bbox",
    })?;
    let bbox = parse_bbox(call, "bbox", bbox_val)?;
    Ok(Box::new(RandomPoint { bbox }))
}

struct RandomPoint {
    bbox: Bbox,
}

impl Generator for RandomPoint {
    fn produce(&mut self, rng: &mut SeedRng, _ctx: &crate::output::RowCtx) -> Cell {
        let lon = rng.gen_range_f64(self.bbox.min_lon, self.bbox.max_lon);
        let lat = rng.gen_range_f64(self.bbox.min_lat, self.bbox.max_lat);
        Cell::Geometry(Geometry::Point { lon, lat })
    }
}

// ---------- randomPointNear ----------------------------------------------

pub fn bind_random_point_near(call: &Call) -> Result<Box<dyn Generator>, SemanticError> {
    require_kwargs_subset(call, &["center", "radius_m"])?;
    if !call.positional.is_empty() {
        return Err(SemanticError::WrongArity {
            line: call.line,
            col: call.col,
            function: call.function.clone(),
            expected: "0 positional arguments".into(),
            got: call.positional.len(),
        });
    }
    let center_val = find_kwarg(call, "center").ok_or_else(|| SemanticError::MissingArg {
        line: call.line,
        col: call.col,
        function: call.function.clone(),
        arg: "center",
    })?;
    // Phase 4.3: `center` accepts either a literal `[lon, lat]` array OR
    // a column reference to a `geometry:point` column. The ref form
    // resolves per-row via `Resolved::resolve` against the materialized
    // parent pool.
    let center: super::resolved::Resolved<(f64, f64)> = match center_val {
        Value::Array(_) => {
            let items = require_array(call, "center", center_val, 2)?;
            let nums = array_as_numbers(call, "center", items)?;
            let (lon, lat) = (nums[0], nums[1]);
            if !(-180.0..=180.0).contains(&lon) || !(-90.0..=90.0).contains(&lat) {
                return Err(SemanticError::InvalidArgValue {
                    line: call.line,
                    col: call.col,
                    function: call.function.clone(),
                    arg: "center".into(),
                    reason: format!("[{lon}, {lat}] is outside WGS84 bounds"),
                });
            }
            super::resolved::Resolved::Literal((lon, lat))
        }
        Value::ColumnRef { table, column } => {
            // The target column's type isn't checked here — the semantic
            // walker only verifies it EXISTS. If the column is not a
            // `geometry:point`, `cast_point` will return `None` at run
            // time and `produce` will panic with a clear message. A
            // future task may tighten this with column-type catalog data.
            super::resolved::Resolved::Ref {
                table: table.clone(),
                column: column.clone(),
                distribution: super::distribution::Distribution::Uniform,
                cast: super::resolved::cast_point,
            }
        }
        other => {
            return Err(SemanticError::TypeMismatch {
                line: call.line,
                col: call.col,
                function: call.function.clone(),
                arg: "center".into(),
                expected: "array [lon, lat] or column reference to a geometry:point",
                got: other.type_name(),
            });
        }
    };

    let radius_val = find_kwarg(call, "radius_m").ok_or_else(|| SemanticError::MissingArg {
        line: call.line,
        col: call.col,
        function: call.function.clone(),
        arg: "radius_m",
    })?;
    let radius_m = expect_number(call, "radius_m", radius_val)?;
    if !(radius_m > 0.0) {
        return Err(SemanticError::InvalidArgValue {
            line: call.line,
            col: call.col,
            function: call.function.clone(),
            arg: "radius_m".into(),
            reason: format!("must be positive, got {radius_m}"),
        });
    }
    Ok(Box::new(RandomPointNear { center, radius_m }))
}

struct RandomPointNear {
    /// Either a fixed `(lon, lat)` decided at bind time, or a column
    /// reference resolved per row from a `geometry:point` column.
    center: super::resolved::Resolved<(f64, f64)>,
    radius_m: f64,
}

impl Generator for RandomPointNear {
    fn produce(&mut self, rng: &mut SeedRng, ctx: &crate::output::RowCtx) -> Cell {
        // Resolve `center` first so the RNG-consumption order is:
        //   (Ref path)     1 draw for parent pick + 2*N draws for the
        //                  rejection-sampled unit disk;
        //   (Literal path) 2*N draws for the rejection-sampled unit disk.
        //
        // Critically, the rejection-sample loop below consumes RNG in the
        // same order as the pre-Task-3.2 code, so the literal-center path
        // is byte-stable.
        let (clon, clat) = self.center.resolve(rng, ctx).unwrap_or_else(|| {
            panic!(
                "randomPointNear: center ref couldn't be resolved (parent pool empty or column not a geometry:point)"
            );
        });

        // Equirectangular approximation: at latitude φ, one degree of
        // longitude is cos(φ) times as long as one degree of latitude.
        // Computed per row because `center` may vary per row when the ref
        // path is in use. For the literal-center path the inputs are
        // constant so the result is bit-stable across rows — no drift.
        // Determinism: `libm::cos` is pure-Rust software libm, so this
        // call is bit-identical across targets.
        const METERS_PER_DEG_LAT: f64 = 111_320.0;
        let lat_rad = clat.to_radians();
        let lon_scale_m_per_deg = METERS_PER_DEG_LAT * libm::cos(lat_rad);
        // Guard against division by zero near the poles. Within 1m of the
        // pole the longitude scaling is meaningless anyway.
        let lon_scale_m_per_deg = lon_scale_m_per_deg.abs().max(1.0);
        let radius_deg_lat = self.radius_m / METERS_PER_DEG_LAT;
        let radius_deg_lon = self.radius_m / lon_scale_m_per_deg;

        // Uniform-by-area sampling via rejection in [-1, 1]². No trig.
        // Acceptance ~78.5%, but we always loop until acceptance — the
        // rejection rate is fully determined by the seed, so determinism
        // is per-seed.
        let (dx, dy) = loop {
            let x = rng.gen_range_f64(-1.0, 1.0);
            let y = rng.gen_range_f64(-1.0, 1.0);
            if x * x + y * y <= 1.0 {
                break (x, y);
            }
        };
        Cell::Geometry(Geometry::Point {
            lon: clon + dx * radius_deg_lon,
            lat: clat + dy * radius_deg_lat,
        })
    }
}

// ---------- randomLineString ---------------------------------------------

pub fn bind_random_line_string(call: &Call) -> Result<Box<dyn Generator>, SemanticError> {
    require_kwargs_subset(call, &["bbox", "segments", "jitter"])?;
    if !call.positional.is_empty() {
        return Err(SemanticError::WrongArity {
            line: call.line,
            col: call.col,
            function: call.function.clone(),
            expected: "0 positional arguments".into(),
            got: call.positional.len(),
        });
    }
    let bbox_val = find_kwarg(call, "bbox").ok_or_else(|| SemanticError::MissingArg {
        line: call.line,
        col: call.col,
        function: call.function.clone(),
        arg: "bbox",
    })?;
    let bbox = parse_bbox(call, "bbox", bbox_val)?;
    let segments = match find_kwarg(call, "segments") {
        Some(v) => expect_integer(call, "segments", v)?,
        None => 5,
    };
    if segments < 1 {
        return Err(SemanticError::InvalidArgValue {
            line: call.line,
            col: call.col,
            function: call.function.clone(),
            arg: "segments".into(),
            reason: format!("must be >= 1, got {segments}"),
        });
    }
    let jitter = match find_kwarg(call, "jitter") {
        Some(v) => expect_number(call, "jitter", v)?,
        None => 0.3,
    };
    if !(0.0..=1.0).contains(&jitter) {
        return Err(SemanticError::InvalidArgValue {
            line: call.line,
            col: call.col,
            function: call.function.clone(),
            arg: "jitter".into(),
            reason: format!("must be in [0.0, 1.0], got {jitter}"),
        });
    }
    Ok(Box::new(RandomLineString {
        bbox,
        segments: segments as usize,
        jitter,
    }))
}

struct RandomLineString {
    bbox: Bbox,
    segments: usize,
    jitter: f64,
}

impl Generator for RandomLineString {
    fn produce(&mut self, rng: &mut SeedRng, _ctx: &crate::output::RowCtx) -> Cell {
        // Start somewhere in the bbox.
        let mut x = rng.gen_range_f64(self.bbox.min_lon, self.bbox.max_lon);
        let mut y = rng.gen_range_f64(self.bbox.min_lat, self.bbox.max_lat);

        // Pick a base heading as a direction vector — no trig: a normalised
        // random vector in [-1, 1]² (rejection-sampled to be inside the
        // unit circle, then normalised by max-coord to keep step sizes
        // sensible). This stays trig-free and platform-stable.
        let (hx, hy) = {
            let mut hx;
            let mut hy;
            loop {
                hx = rng.gen_range_f64(-1.0, 1.0);
                hy = rng.gen_range_f64(-1.0, 1.0);
                let mag2 = hx * hx + hy * hy;
                if mag2 > 0.0001 && mag2 <= 1.0 {
                    let mag = mag2.sqrt(); // f64::sqrt is bit-stable (IEEE 754)
                    hx /= mag;
                    hy /= mag;
                    break;
                }
            }
            (hx, hy)
        };

        // Step size: span the bbox roughly evenly across segments.
        let step_lon = (self.bbox.max_lon - self.bbox.min_lon) / self.segments as f64 * 0.7;
        let step_lat = (self.bbox.max_lat - self.bbox.min_lat) / self.segments as f64 * 0.7;
        let step = step_lon.hypot(step_lat); // hypot is also IEEE 754 stable

        let mut verts = Vec::with_capacity(self.segments + 1);
        verts.push((x, y));

        for _ in 0..self.segments {
            // Perturb the heading. jitter=0 → no change; jitter=1 → full
            // random walk.
            let dx = rng.gen_range_f64(-1.0, 1.0) * self.jitter;
            let dy = rng.gen_range_f64(-1.0, 1.0) * self.jitter;
            let nhx = hx * (1.0 - self.jitter) + dx;
            let nhy = hy * (1.0 - self.jitter) + dy;
            let nmag = (nhx * nhx + nhy * nhy).sqrt().max(1e-9);
            let ux = nhx / nmag;
            let uy = nhy / nmag;
            x = (x + ux * step).clamp(self.bbox.min_lon, self.bbox.max_lon);
            y = (y + uy * step).clamp(self.bbox.min_lat, self.bbox.max_lat);
            verts.push((x, y));
        }

        Cell::Geometry(Geometry::LineString(verts))
    }
}

// ---------- randomPolygon -------------------------------------------------

pub fn bind_random_polygon(call: &Call) -> Result<Box<dyn Generator>, SemanticError> {
    require_kwargs_subset(call, &["bbox", "vertices", "irregularity"])?;
    if !call.positional.is_empty() {
        return Err(SemanticError::WrongArity {
            line: call.line,
            col: call.col,
            function: call.function.clone(),
            expected: "0 positional arguments".into(),
            got: call.positional.len(),
        });
    }
    let bbox_val = find_kwarg(call, "bbox").ok_or_else(|| SemanticError::MissingArg {
        line: call.line,
        col: call.col,
        function: call.function.clone(),
        arg: "bbox",
    })?;
    let bbox = parse_bbox(call, "bbox", bbox_val)?;
    let vertices = match find_kwarg(call, "vertices") {
        Some(v) => expect_integer(call, "vertices", v)?,
        None => 6,
    };
    if vertices < 3 {
        return Err(SemanticError::InvalidArgValue {
            line: call.line,
            col: call.col,
            function: call.function.clone(),
            arg: "vertices".into(),
            reason: format!("must be >= 3, got {vertices}"),
        });
    }
    let irregularity = match find_kwarg(call, "irregularity") {
        Some(v) => expect_number(call, "irregularity", v)?,
        None => 0.3,
    };
    if !(0.0..=1.0).contains(&irregularity) {
        return Err(SemanticError::InvalidArgValue {
            line: call.line,
            col: call.col,
            function: call.function.clone(),
            arg: "irregularity".into(),
            reason: format!("must be in [0.0, 1.0], got {irregularity}"),
        });
    }
    Ok(Box::new(RandomPolygon {
        bbox,
        vertices: vertices as usize,
        irregularity,
    }))
}

struct RandomPolygon {
    bbox: Bbox,
    vertices: usize,
    irregularity: f64,
}

impl Generator for RandomPolygon {
    fn produce(&mut self, rng: &mut SeedRng, _ctx: &crate::output::RowCtx) -> Cell {
        // Pick a random centroid inside the bbox.
        let cx = rng.gen_range_f64(self.bbox.min_lon, self.bbox.max_lon);
        let cy = rng.gen_range_f64(self.bbox.min_lat, self.bbox.max_lat);

        // Maximum radius that fits inside the bbox in degrees (axis-min).
        let half_w = (self.bbox.max_lon - self.bbox.min_lon) * 0.5;
        let half_h = (self.bbox.max_lat - self.bbox.min_lat) * 0.5;
        let max_r = (cx - self.bbox.min_lon)
            .min(self.bbox.max_lon - cx)
            .min(cy - self.bbox.min_lat)
            .min(self.bbox.max_lat - cy)
            .min(half_w)
            .min(half_h)
            .max(1e-6);

        // For the angle assignment we deliberately avoid trig: instead of
        // `cos(θ)`/`sin(θ)`, we walk the boundary of an axis-aligned unit
        // square at fractional positions `t ∈ [0, 1)` and use that as the
        // direction. This gives a roughly evenly-spaced star-polygon in
        // direction space and stays bit-stable across targets.
        let mut steps: Vec<f64> = (0..self.vertices)
            .map(|i| {
                let base = i as f64 / self.vertices as f64;
                let jitter =
                    rng.gen_range_f64(-0.5, 0.5) * self.irregularity / self.vertices as f64;
                (base + jitter).rem_euclid(1.0)
            })
            .collect();
        steps.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        // Map t ∈ [0,1) to a direction (dx, dy) on the unit square's
        // boundary, then scale by radius. Direction is CCW around the
        // centroid (right-hand rule: +x first quadrant first).
        let direction = |t: f64| -> (f64, f64) {
            let s = t * 4.0;
            let seg = s as usize % 4;
            let f = s - seg as f64;
            match seg {
                0 => (1.0 - 2.0 * f, 1.0),       // top edge, right→left
                1 => (-1.0, 1.0 - 2.0 * f),      // left edge, top→bottom
                2 => (-1.0 + 2.0 * f, -1.0),     // bottom, left→right
                _ => (1.0, -1.0 + 2.0 * f),      // right edge, bottom→top
            }
        };

        let min_r = max_r * (1.0 - self.irregularity * 0.7);
        let mut ring: Vec<(f64, f64)> = steps
            .into_iter()
            .map(|t| {
                let r = if self.irregularity == 0.0 {
                    max_r
                } else {
                    rng.gen_range_f64(min_r, max_r)
                };
                let (dx, dy) = direction(t);
                let x = (cx + dx * r).clamp(self.bbox.min_lon, self.bbox.max_lon);
                let y = (cy + dy * r).clamp(self.bbox.min_lat, self.bbox.max_lat);
                (x, y)
            })
            .collect();

        // Force CCW orientation. If the signed area is negative (CW), reverse.
        if signed_area(&ring) < 0.0 {
            ring.reverse();
        }
        // Close the ring — first vertex == last vertex.
        if let Some(first) = ring.first().copied() {
            ring.push(first);
        }

        Cell::Geometry(Geometry::Polygon {
            rings: vec![ring],
        })
    }
}

/// Shoelace formula. Positive for CCW rings, negative for CW.
fn signed_area(ring: &[(f64, f64)]) -> f64 {
    let n = ring.len();
    if n < 3 {
        return 0.0;
    }
    let mut sum = 0.0;
    for i in 0..n {
        let (x1, y1) = ring[i];
        let (x2, y2) = ring[(i + 1) % n];
        sum += x1 * y2 - x2 * y1;
    }
    sum * 0.5
}

// ---------- randomBbox ----------------------------------------------------

pub fn bind_random_bbox(call: &Call) -> Result<Box<dyn Generator>, SemanticError> {
    require_kwargs_subset(call, &["within", "min_size_deg", "max_size_deg"])?;
    if !call.positional.is_empty() {
        return Err(SemanticError::WrongArity {
            line: call.line,
            col: call.col,
            function: call.function.clone(),
            expected: "0 positional arguments".into(),
            got: call.positional.len(),
        });
    }
    let within_val = find_kwarg(call, "within").ok_or_else(|| SemanticError::MissingArg {
        line: call.line,
        col: call.col,
        function: call.function.clone(),
        arg: "within",
    })?;
    let within = parse_bbox(call, "within", within_val)?;
    let min_size = match find_kwarg(call, "min_size_deg") {
        Some(v) => expect_number(call, "min_size_deg", v)?,
        None => 0.01,
    };
    let max_size = match find_kwarg(call, "max_size_deg") {
        Some(v) => expect_number(call, "max_size_deg", v)?,
        None => 0.5,
    };
    if !(min_size > 0.0) {
        return Err(SemanticError::InvalidArgValue {
            line: call.line,
            col: call.col,
            function: call.function.clone(),
            arg: "min_size_deg".into(),
            reason: format!("must be > 0, got {min_size}"),
        });
    }
    if min_size > max_size {
        return Err(SemanticError::InvalidArgValue {
            line: call.line,
            col: call.col,
            function: call.function.clone(),
            arg: "min_size_deg".into(),
            reason: format!(
                "min_size_deg ({min_size}) must be <= max_size_deg ({max_size})"
            ),
        });
    }
    // Clamp max_size_deg down to whatever fits inside `within`. This is the
    // "warn at semantic-check time" behaviour the spec calls for — we
    // silently clamp here; CLI surface could surface a stderr warning later.
    let avail_w = within.max_lon - within.min_lon;
    let avail_h = within.max_lat - within.min_lat;
    let max_fit = avail_w.min(avail_h);
    if max_fit < min_size {
        return Err(SemanticError::InvalidArgValue {
            line: call.line,
            col: call.col,
            function: call.function.clone(),
            arg: "within".into(),
            reason: format!(
                "within bbox is too small ({avail_w}° × {avail_h}°) to fit min_size_deg={min_size}"
            ),
        });
    }
    let clamped_max = max_size.min(max_fit);
    Ok(Box::new(RandomBboxGen {
        within,
        min_size,
        max_size: clamped_max,
    }))
}

struct RandomBboxGen {
    within: Bbox,
    min_size: f64,
    max_size: f64,
}

impl Generator for RandomBboxGen {
    fn produce(&mut self, rng: &mut SeedRng, _ctx: &crate::output::RowCtx) -> Cell {
        let w = rng.gen_range_f64(self.min_size, self.max_size);
        let h = rng.gen_range_f64(self.min_size, self.max_size);
        let min_lon = rng.gen_range_f64(self.within.min_lon, self.within.max_lon - w);
        let min_lat = rng.gen_range_f64(self.within.min_lat, self.within.max_lat - h);
        let max_lon = min_lon + w;
        let max_lat = min_lat + h;
        // Counter-clockwise from lower-left.
        let ring = vec![
            (min_lon, min_lat),
            (max_lon, min_lat),
            (max_lon, max_lat),
            (min_lon, max_lat),
            (min_lon, min_lat),
        ];
        Cell::Geometry(Geometry::Polygon { rings: vec![ring] })
    }
}
