//! WGS84 geometry value type, shared between generators (which produce it)
//! and the output layer (which serialises it as WKT, GeoJSON, or EWKT).
//!
//! Coordinates are kept as raw `f64` internally; rounding to 7 decimal
//! places (≈ 1 cm at the equator) happens at output time so the value
//! itself stays a pure representation rather than a presentation choice.

mod ewkt;
mod geojson;
mod wkt;

pub use ewkt::write_ewkt;
pub use geojson::to_geojson;
pub use wkt::write_wkt;

#[derive(Debug, Clone, PartialEq)]
pub enum Geometry {
    Point { lon: f64, lat: f64 },
    LineString(Vec<(f64, f64)>),
    /// `rings[0]` is the exterior ring; `rings[1..]` would be holes (Phase 3+).
    /// Phase 2 generators only ever produce a single ring. The exterior ring
    /// is always counter-clockwise (right-hand rule, GeoJSON convention) and
    /// closed (first vertex equals last vertex).
    Polygon { rings: Vec<Vec<(f64, f64)>> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeometryKind {
    Point,
    LineString,
    Polygon,
}

impl GeometryKind {
    /// String form used in `dataseed functions --json` for the `returns` field.
    pub fn as_return_type(&self) -> &'static str {
        match self {
            GeometryKind::Point => "geometry:point",
            GeometryKind::LineString => "geometry:linestring",
            GeometryKind::Polygon => "geometry:polygon",
        }
    }
}

impl Geometry {
    pub fn kind(&self) -> GeometryKind {
        match self {
            Geometry::Point { .. } => GeometryKind::Point,
            Geometry::LineString(_) => GeometryKind::LineString,
            Geometry::Polygon { .. } => GeometryKind::Polygon,
        }
    }
}

/// Round to 7 decimal places (≈ 1 cm at the equator). The result is still
/// an f64 so JSON serialisers can emit it as a number, not a string.
pub(crate) fn round_coord(x: f64) -> f64 {
    (x * 1e7).round() / 1e7
}

/// Format a coordinate for textual output (WKT/EWKT). Rounds first, then
/// uses Rust's default `Display`, which is Ryu-based and deterministic
/// across platforms. `5.1` stays `"5.1"`; `5.1234567899` becomes `"5.1234568"`.
pub(crate) fn fmt_coord(x: f64) -> String {
    format!("{}", round_coord(x))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_coord_truncates_to_7_decimals() {
        assert_eq!(round_coord(5.12345678999), 5.1234568);
        assert_eq!(round_coord(5.1), 5.1);
        assert_eq!(round_coord(-0.000000049), 0.0);
        assert_eq!(round_coord(-0.00000006), -0.0000001);
    }

    #[test]
    fn fmt_coord_uses_shortest_round_trip() {
        assert_eq!(fmt_coord(5.1), "5.1");
        assert_eq!(fmt_coord(5.12345678999), "5.1234568");
        assert_eq!(fmt_coord(0.0), "0");
    }

    #[test]
    fn kind_matches_variant() {
        assert_eq!(
            Geometry::Point { lon: 0.0, lat: 0.0 }.kind(),
            GeometryKind::Point
        );
        assert_eq!(
            Geometry::LineString(vec![(0.0, 0.0), (1.0, 1.0)]).kind(),
            GeometryKind::LineString
        );
        assert_eq!(
            Geometry::Polygon {
                rings: vec![vec![(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 0.0)]]
            }
            .kind(),
            GeometryKind::Polygon
        );
    }

    #[test]
    fn return_type_strings_match_catalog_convention() {
        assert_eq!(GeometryKind::Point.as_return_type(), "geometry:point");
        assert_eq!(
            GeometryKind::LineString.as_return_type(),
            "geometry:linestring"
        );
        assert_eq!(GeometryKind::Polygon.as_return_type(), "geometry:polygon");
    }
}
